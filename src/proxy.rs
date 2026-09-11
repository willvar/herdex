//! Codex client-facing routes: pure pass-through router with pool-aware
//! auth swapping. Request bodies are forwarded byte-for-byte, so codex wire
//! features (reasoning, service tier, encrypted CoT) are inherited for free.

use crate::app::{App, AppHandle};
use crate::pool;
use crate::store::{Account, LogEntry};
use axum::body::{Body, Bytes};
use axum::extract::{State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const HOP_HEADERS: [&str; 9] = [
    "connection",
    "proxy-connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

pub fn router() -> axum::Router<AppHandle> {
    axum::Router::new()
        .route("/v1/models", axum::routing::get(models))
        .route(
            "/v1/responses",
            axum::routing::get(ws_upgrade).post(responses),
        )
        .route(
            "/backend-api/codex/responses",
            axum::routing::get(ws_upgrade).post(responses),
        )
}

fn auth_check(app: &App, headers: &HeaderMap) -> Result<String, Response> {
    let key = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    if key.is_empty() {
        return Err((StatusCode::UNAUTHORIZED, "missing bearer key").into_response());
    }
    match app.store.api_key_valid(&key) {
        Ok(true) => Ok(key),
        Ok(false) => Err((StatusCode::UNAUTHORIZED, "invalid api key").into_response()),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e).into_response()),
    }
}

async fn models(State(app): State<AppHandle>, headers: HeaderMap) -> Response {
    if auth_check(&app, &headers).is_err() {
        return (StatusCode::UNAUTHORIZED, "invalid api key").into_response();
    }
    let data: Vec<serde_json::Value> = crate::store::MODEL_CATALOG
        .iter()
        .map(|m| serde_json::json!({"id": m, "object": "model", "owned_by": "openai"}))
        .collect();
    axum::Json(serde_json::json!({"object": "list", "data": data})).into_response()
}

/// Decides whether one account's failure should move to the next.
fn failoverable(status: u16, body: &str) -> bool {
    match status {
        401 | 403 | 408 | 429 | 500 | 502 | 503 | 504 => true,
        // plan-gated models reject per-account (e.g. spark on plus); try next
        400 => body.contains("not supported"),
        _ => false,
    }
}

fn upstream_status(last: u16) -> StatusCode {
    StatusCode::from_u16(if last >= 400 { last } else { 502 }).unwrap_or(StatusCode::BAD_GATEWAY)
}

async fn responses(
    State(app): State<AppHandle>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let api_key = match auth_check(&app, &headers) {
        Ok(k) => k,
        Err(res) => return res,
    };
    let probe: Result<serde_json::Value, _> = serde_json::from_slice(&body);
    let model = match probe.ok().and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from)) {
        Some(m) => m,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                r#"{"error":{"message":"missing \"model\" in request body","type":"server_error"}}"#,
            )
                .into_response()
        }
    };

    let candidates = match app.pool.candidates(&model) {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    if candidates.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("no enabled account can serve model {model}"),
        )
            .into_response();
    }
    // never-blackout: every eligible candidate gets exactly one attempt;
    // cooled accounts are re-tried only after the pool is exhausted once
    let attempts = candidates.len();

    let session_key = headers
        .get("Session-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let mut last_status: u16 = 0;
    let mut last_err = String::new();
    let start = Instant::now();
    for attempt in 0..attempts {
        let mut acc = candidates[attempt % candidates.len()].clone();
        if (acc.expires_at - crate::store::now_secs()) < 300 {
            if let Err(e) = app.refresh_expired(&mut acc).await {
                app.pool.mark_failure(&acc.id, &model);
                last_status = 401;
                last_err = format!("token refresh failed for {}: {e}", acc.email);
                continue;
            }
        }
        match attempt_once(&app, &headers, &body, &acc, &model, start, &session_key, &api_key).await {
            Ok(resp) => return resp,
            Err((status, msg)) => {
                last_status = status;
                last_err = msg;
                if !failoverable(last_status, &last_err) {
                    break;
                }
            }
        }
    }
    let body = serde_json::json!({
        "error": {"message": format!("all pool attempts failed; last: {last_err}"), "type": "server_error"}
    });
    (upstream_status(last_status), axum::Json(body)).into_response()
}

type AttemptResult = Result<Response, (u16, String)>;

async fn attempt_once(
    app: &App,
    client_headers: &HeaderMap,
    body: &[u8],
    acc: &Account,
    model: &str,
    start: Instant,
    session_key: &str,
    api_key: &str,
) -> AttemptResult {
    let url = format!("{}/responses", app.cfg.upstream.base_url.trim_end_matches('/'));
    // Self-healing body: when the backend rejects a named parameter, strip it
    // and retry the same account. Genuine codex requests are accepted as-is,
    // so the byte-for-byte passthrough invariant is untouched in practice.
    let mut body = body.to_vec();
    let codex_client = App::is_codex_client(client_headers);
    if !codex_client {
        // apply learned strips up-front so steady state costs no extra
        // upstream request (only the first request after each discovery pays)
        let learned = app.learned_strips.lock().unwrap().clone();
        for p in &learned {
            if let Some(nb) = remove_param(&body, p) {
                body = nb;
            }
        }
    }
    let res = loop {
        let rb = app
            .http
            .post(&url)
            .header("Content-Type", client_headers.get("Content-Type").and_then(|v| v.to_str().ok()).unwrap_or("application/json"))
            .header("Accept", client_headers.get("Accept").and_then(|v| v.to_str().ok()).unwrap_or("text/event-stream"))
            .bearer_auth(&acc.access_token)
            .header("Chatgpt-Account-Id", &acc.account_id);
        let rb = app.apply_identity(rb, client_headers);
        let res = rb.body(body.clone()).send().await;
        let res = match res {
            Ok(r) => r,
            Err(e) => {
                app.pool.mark_failure(&acc.id, model);
                return Err((0, format!("{}: {e}", acc.email)));
            }
        };
        if res.status().as_u16() == 400 {
            let headers = res.headers().clone();
            let text = res.text().await.unwrap_or_default();
            if let Some(param) = rejected_param(&text) {
                if let Some(nb) = remove_param(&body, &param) {
                    log::info!("stripping rejected param {param} and retrying");
                    if !codex_client {
                        app.learn_strip(param.clone());
                    }
                    body = nb;
                    continue;
                }
            }
            // genuine 400: rebuild the passthrough response so the normal
            // status>=400 handling below sees it unchanged
            let mut hr = http::Response::builder().status(axum::http::StatusCode::BAD_REQUEST);
            for (k, v) in &headers {
                hr = hr.header(k.as_str(), v.as_bytes());
            }
            let built = hr
                .body(text)
                .map_err(|e| (500, e.to_string()))?;
            break reqwest::Response::from(built);
        }
        break res;
    };
    let latency = start.elapsed().as_millis() as i64;

    // quota observation from upstream response headers
    let pri = header_pct(res.headers(), "x-codex-bengalfox-primary-used-percent");
    let sec = header_pct(res.headers(), "x-codex-bengalfox-secondary-used-percent");
    let pri_reset = reset_epoch(res.headers(), "x-codex-bengalfox-primary-reset-at", "x-codex-bengalfox-primary-reset-after-seconds");
    let sec_reset = reset_epoch(res.headers(), "x-codex-bengalfox-secondary-reset-at", "x-codex-bengalfox-secondary-reset-after-seconds");
    let pri_secs = header_minutes(res.headers(), "x-codex-bengalfox-primary-window-minutes");
    let sec_secs = header_minutes(res.headers(), "x-codex-bengalfox-secondary-window-minutes");
    if pri.is_some() || sec.is_some() || pri_reset.is_some() || sec_reset.is_some() {
        app.pool.observe(
            &acc.id,
            model,
            pool::Quota {
                primary_pct: pri.unwrap_or(0.0),
                secondary_pct: sec.unwrap_or(0.0),
                primary_reset_at: pri_reset.unwrap_or(0),
                secondary_reset_at: sec_reset.unwrap_or(0),
                primary_window_secs: pri_secs.unwrap_or(0),
                secondary_window_secs: sec_secs.unwrap_or(0),
                observed_at: crate::store::now_secs(),
            },
        );
    }

    let status = res.status().as_u16();
    let resp_headers = res.headers().clone();
    if status >= 400 {
        let snippet = res
            .text()
            .await
            .unwrap_or_default();
        if failoverable(status, &snippet) {
            app.pool.mark_failure(&acc.id, model);
            log::info!("{model} -> {status} ({}ms) via {}: {}", start.elapsed().as_millis(), acc.email, truncate(&snippet));
            return Err((status, format!("{}: {status} {snippet}", acc.email)));
        }
        // genuine client error: pass through untouched
        let mut out = Response::builder().status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST));
        copy_headers(out.headers_mut().unwrap(), &resp_headers);
        log::info!("{model} -> {status} ({}ms) via {}", start.elapsed().as_millis(), acc.email);
        return out.body(Body::from(snippet)).map_err(|e| (500, e.to_string()));
    }

    // success: stream to client, log usage after the stream completes
    app.pool.mark_used(&acc.id);
    app.pool.pin(session_key, &acc.id, model);
    let mut out = Response::builder().status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK));
    copy_headers(out.headers_mut().unwrap(), &resp_headers);

    let tail: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let log_tail = tail.clone();
    let log_model = model.to_string();
    let log_email = acc.email.clone();
    let log_key = api_key.to_string();
    let log_status = status as i64;
    let log_latency = latency;
    let store = app.store.clone();
    let stream = async_stream::stream! {
        let mut res = res;
        while let Some(chunk) = res.chunk().await.transpose() {
            match chunk {
                Ok(c) => {
                    {
                        let mut t = log_tail.lock().unwrap();
                        t.extend_from_slice(&c);
                        if t.len() > 64 * 1024 {
                            let drop = t.len() - 64 * 1024;
                            t.drain(..drop);
                        }
                    }
                    yield Ok::<_, std::io::Error>(c);
                }
                Err(e) => {
                    yield Err(std::io::Error::new(std::io::ErrorKind::Other, e));
                    break;
                }
            }
        }
        let t = log_tail.lock().unwrap();
        let input = extract_int(&t, "\"input_tokens\":");
        let cached = extract_int(&t, "\"cached_tokens\":");
        let output = extract_int(&t, "\"output_tokens\":");
        drop(t);
        store.add_log(&LogEntry {
            ts: crate::store::now_secs(),
            account_email: log_email,
            api_key: log_key,
            model: log_model,
            status: log_status,
            latency_ms: log_latency,
            input_tokens: input,
            cached_tokens: cached,
            output_tokens: output,
            error: String::new(),
        });
    };
    out.body(Body::from_stream(stream)).map_err(|e| (500, e.to_string()))
}

fn header_pct(h: &reqwest::header::HeaderMap, name: &str) -> Option<f64> {
    h.get(name)?.to_str().ok()?.trim_end_matches('%').parse().ok()
}

fn header_minutes(h: &reqwest::header::HeaderMap, name: &str) -> Option<i64> {
    let m: i64 = h.get(name)?.to_str().ok()?.parse().ok()?;
    (m > 0).then(|| m * 60)
}

fn reset_epoch(h: &reqwest::header::HeaderMap, at_key: &str, after_key: &str) -> Option<i64> {
    if let Some(v) = h.get(at_key).and_then(|v| v.to_str().ok()) {
        if let Ok(sec) = v.parse::<i64>() {
            if sec > 0 {
                return Some(sec);
            }
        }
    }
    if let Some(v) = h.get(after_key).and_then(|v| v.to_str().ok()) {
        if let Ok(sec) = v.parse::<i64>() {
            if sec > 0 {
                return Some(crate::store::now_secs() + sec);
            }
        }
    }
    None
}

fn extract_int(data: &[u8], key: &str) -> i64 {
    let Some(pos) = data
        .windows(key.len())
        .rposition(|w| w == key.as_bytes())
    else {
        return 0;
    };
    let rest = &data[pos + key.len()..];
    let end = rest.iter().position(|b| !b.is_ascii_digit()).unwrap_or(rest.len());
    std::str::from_utf8(&rest[..end])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn copy_headers(dst: &mut axum::http::HeaderMap, src: &reqwest::header::HeaderMap) {
    for (k, v) in src.iter() {
        if HOP_HEADERS.contains(&k.as_str()) || k == "content-length" {
            continue;
        }
        if let (Ok(name), Ok(val)) = (
            axum::http::HeaderName::from_bytes(k.as_str().as_bytes()),
            axum::http::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            dst.insert(name, val);
        }
    }
}

fn truncate(s: &str) -> String {
    s.chars().take(200).collect()
}

/// Extracts the parameter name from a backend 400 rejection that names it,
/// e.g. `{"detail":"Unsupported parameter: max_output_tokens"}`. Returns
/// None for any other error shape — only self-describing rejections are
/// auto-corrected.
fn rejected_param(snippet: &str) -> Option<String> {
    for marker in ["Unsupported parameter: ", "Unrecognized request argument supplied: "] {
        if let Some(pos) = snippet.find(marker) {
            let rest = &snippet[pos + marker.len()..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.' || *c == '-')
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// Removes a (possibly dotted) path from a JSON body. Returns None if the
/// body is not JSON or the path is absent.
fn remove_param(body: &[u8], path: &str) -> Option<Vec<u8>> {
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let mut obj = v.as_object_mut()?;
    let mut parts = path.split('.').peekable();
    while let Some(p) = parts.next() {
        if parts.peek().is_none() {
            obj.remove(p)?;
        } else {
            obj = obj.get_mut(p)?.as_object_mut()?;
        }
    }
    serde_json::to_vec(&v).ok()
}

// ---- WebSocket passthrough ----

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(app): State<AppHandle>,
    headers: HeaderMap,
) -> Response {
    let candidates = app.pool.candidates("").unwrap_or_default();
    let session_key = headers
        .get("Session-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let mut candidates = candidates;
    if let Some(pinned) = app.pool.pinned(&session_key, "") {
        if let Some(i) = candidates.iter().position(|c| c.id == pinned) {
            if i != 0 {
                candidates.swap(0, i);
            }
        }
    }
    let Some(acc) = candidates.into_iter().next() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no account available for websocket").into_response();
    };
    ws.on_upgrade(move |sock| async move { pipe(app, sock, headers, acc).await })
}

async fn pipe(app: AppHandle, sock: axum::extract::ws::WebSocket, headers: HeaderMap, acc: crate::store::Account) {
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let mut upstream_url = app.cfg.upstream.base_url.trim_end_matches('/').to_string();
    upstream_url = upstream_url.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
    upstream_url.push_str("/responses");

    let Ok(mut req) = upstream_url.as_str().into_client_request() else {
        return;
    };
    use tokio_tungstenite::tungstenite::http::HeaderValue as TsHeaderValue;
    if let Ok(v) = TsHeaderValue::from_str(&format!("Bearer {}", acc.access_token)) {
        req.headers_mut().insert("Authorization", v);
    }
    if !acc.account_id.is_empty() {
        if let Ok(v) = TsHeaderValue::from_str(&acc.account_id) {
            req.headers_mut().insert("Chatgpt-Account-Id", v);
        }
    }
    for (k, v) in &app.cfg.header_defaults {
        if let (Ok(name), Ok(val)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            v.parse::<reqwest::header::HeaderValue>(),
        ) {
            req.headers_mut().insert(name, val);
        }
    }
    if let Some(v) = headers.get("Session-Id").and_then(|v| v.to_str().ok()) {
        if let Ok(v) = TsHeaderValue::from_str(v) {
            req.headers_mut().insert("Session-Id", v);
        }
    }

    let (mut up_tx, mut up_rx) = match tokio_tungstenite::connect_async(req).await {
        Ok((ws, _)) => ws.split(),
        Err(_) => return,
    };
    let (mut cl_tx, mut cl_rx) = sock.split();

    let c2u = tokio::spawn(async move {
        while let Some(Ok(msg)) = cl_rx.next().await {
            let out = match msg {
                axum::extract::ws::Message::Text(t) => WsMessage::Text(t.to_string()),
                axum::extract::ws::Message::Binary(b) => WsMessage::Binary(b.to_vec()),
                axum::extract::ws::Message::Ping(b) => WsMessage::Ping(b.to_vec()),
                axum::extract::ws::Message::Pong(b) => WsMessage::Pong(b.to_vec()),
                axum::extract::ws::Message::Close(f) => {
                    use tokio_tungstenite::tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};
                    WsMessage::Close(f.map(|f| CloseFrame {
                        code: CloseCode::from(f.code),
                        reason: f.reason.to_string().into(),
                    }))
                }
            };
            if up_tx.send(out).await.is_err() {
                break;
            }
        }
    });
    let u2c = tokio::spawn(async move {
        while let Some(Ok(msg)) = up_rx.next().await {
            let out = match msg {
                WsMessage::Text(t) => axum::extract::ws::Message::Text(t.as_str().into()),
                WsMessage::Binary(b) => axum::extract::ws::Message::Binary(b.into()),
                WsMessage::Ping(b) => axum::extract::ws::Message::Ping(b.into()),
                WsMessage::Pong(b) => axum::extract::ws::Message::Pong(b.into()),
                WsMessage::Close(f) => axum::extract::ws::Message::Close(f.map(|f| {
                    axum::extract::ws::CloseFrame {
                        code: u16::from(f.code),
                        reason: f.reason.to_string().as_str().into(),
                    }
                })),
                _ => continue,
            };
            if cl_tx.send(out).await.is_err() {
                break;
            }
        }
        let _ = cl_tx.close().await;
    });
    let _ = c2u.await;
    let _ = u2c.await;
    app.pool.mark_used(&acc.id);
}

use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_param_extracts_named_params() {
        assert_eq!(
            rejected_param(r#"{"detail":"Unsupported parameter: max_output_tokens"}"#),
            Some("max_output_tokens".to_string())
        );
        assert_eq!(
            rejected_param(r#"{"error":{"message":"Unrecognized request argument supplied: stream_options"}}"#),
            Some("stream_options".to_string())
        );
        assert_eq!(rejected_param(r#"{"detail":"Invalid value"}"#), None);
    }

    #[test]
    fn remove_param_removes_dotted_paths() {
        let body = br#"{"a":1,"reasoning":{"effort":"high"},"max_output_tokens":5}"#;
        let out: serde_json::Value =
            serde_json::from_slice(&remove_param(body, "max_output_tokens").unwrap()).unwrap();
        assert!(out.get("max_output_tokens").is_none());
        assert_eq!(out["a"], 1);
        let out: serde_json::Value = serde_json::from_slice(
            &remove_param(body, "reasoning.effort").unwrap(),
        )
        .unwrap();
        assert!(out["reasoning"].get("effort").is_none());
        assert!(remove_param(body, "missing.path").is_none());
        assert!(remove_param(b"not json", "x").is_none());
    }
}
