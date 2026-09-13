//! Codex client-facing routes: pure pass-through router with pool-aware
//! auth swapping. Request bodies are forwarded byte-for-byte, so codex wire
//! features (reasoning, service tier, encrypted CoT) are inherited for free.

use crate::app::{App, AppHandle};
use crate::pool;
use crate::store::{Account, LogEntry};
use axum::body::{Body, Bytes};
use axum::extract::{State, WebSocketUpgrade};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
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
        // codex CLI's statusline is fed exclusively by its periodic account
        // usage poll (GET {base}/api/codex/usage for CodexApi-path base URLs);
        // without this route the poll 404s and the statusline freezes
        .route("/api/codex/usage", axum::routing::get(codex_usage))
        .route("/v1/api/codex/usage", axum::routing::get(codex_usage))
        .route("/wham/usage", axum::routing::get(codex_usage))
        .route("/v1/user-auth-credential/whoami", axum::routing::get(whoami))
        .route("/user-auth-credential/whoami", axum::routing::get(whoami))
        // apps MCP: forwarded with pool-account credentials (the same auth
        // shape /responses uses) because the CLI's own OAuth forwarded from
        // behind the gateway gets Cloudflare-challenged
        .route("/api/codex/ps/mcp", axum::routing::any(codex_apps_mcp))
        .route("/v1/api/codex/ps/mcp", axum::routing::any(codex_apps_mcp))
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

/// Pool-aggregate usage poll ("池子总量"): probes every enabled account and
/// reports the pool as ONE virtual account — used% is the capacity-weighted
/// mean of per-account usage, so it stays within 0-100 even when accounts
/// differ by plan. Weights come from the empirical tokens-per-1% calibration;
/// uncalibrated accounts fall back to the mean calibrated weight (or equal
/// weight when nothing is calibrated yet). reset_at = earliest window reset.
async fn codex_usage_pool(app: &App) -> Response {
    let accounts = app.store.list_accounts().unwrap_or_default();
    let mut collected: Vec<(Account, crate::usage::Report)> = Vec::new();
    for a in accounts.iter().filter(|a| !a.disabled) {
        let body = match crate::usage::fetch_raw(
            &app.http,
            &app.usage_root,
            &a.access_token,
            &a.account_id,
            &app.cfg.header_defaults,
        )
        .await
        {
            Ok(b) => b,
            Err(_) => continue,
        };
        if let Ok(report) = crate::usage::parse(body.as_bytes()) {
            if !a.plan_type.is_empty()
                && !report.plan_type.is_empty()
                && report.plan_type != a.plan_type
            {
                log::info!(
                    "plan changed for {}: {} -> {}",
                    a.email, a.plan_type, report.plan_type
                );
                let _ = app.store.set_account_plan(&a.id, &report.plan_type);
            }
            app.observe_usage(&a.id, &report);
            collected.push((a.clone(), report));
        }
    }
    if collected.is_empty() {
        return (StatusCode::BAD_GATEWAY, "no account usage probe succeeded").into_response();
    }

    // weight = calibrated capacity (tokens per 1%); unknown → fallback weight
    struct Entry {
        weight: Option<f64>,
        pct: f64,
        reset_at: i64,
        win_secs: i64,
        plan: String,
    }
    let mut entries: Vec<Entry> = Vec::new();
    for (a, rep) in &collected {
        let pct = rep
            .main
            .primary
            .used_pct
            .unwrap_or(0.0)
            .max(rep.main.secondary.used_pct.unwrap_or(0.0));
        let weight = app
            .store
            .calibration(&a.id)
            .ok()
            .flatten()
            .map(|c| c.tokens_per_pct);
        entries.push(Entry {
            weight,
            pct,
            reset_at: rep.main.primary.reset_at.unwrap_or(0),
            win_secs: rep.main.primary.window_seconds.unwrap_or(604800),
            plan: rep.plan_type.clone(),
        });
    }
    let used = weighted_used(
        &entries.iter().map(|e| (e.pct, e.weight)).collect::<Vec<_>>(),
    );
    let mut min_reset = i64::MAX;
    let mut win = 604800i64;
    let mut plan = "pro".to_string();
    let mut best_pct = f64::MAX;
    for e in &entries {
        if e.reset_at > 0 {
            min_reset = min_reset.min(e.reset_at);
        }
        if e.win_secs > win {
            win = e.win_secs;
        }
        if e.pct < best_pct {
            best_pct = e.pct;
            plan = e.plan.clone();
        }
    }
    let now = crate::store::now_secs();
    // Shape must match the CLI's RateLimitStatusPayload exactly: used_percent
    // and window fields deserialize as i32 (floats fail the whole response),
    // and account_id/user_id must equal the PAT whoami identity or the TUI
    // filters ordinary_usage_allowed out of the /status card.
    let payload = serde_json::json!({
        "plan_type": plan,
        "rate_limit": {
            "allowed": true,
            "limit_reached": used >= 99.9,
            "primary_window": {
                "used_percent": used.round() as i64,
                "limit_window_seconds": win,
                "reset_at": if min_reset == i64::MAX { 0 } else { min_reset },
                "reset_after_seconds": if min_reset == i64::MAX { 0 } else { (min_reset - now).max(0) },
            },
        },
        "rate_limit_reset_credits": { "available_count": 0 },
        "account_id": POOL_IDENTITY.account_id,
        "user_id": POOL_IDENTITY.user_id,
    });
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        payload.to_string(),
    )
        .into_response()
}

/// The virtual ChatGPT identity herdex presents to the CLI's PAT auth flow.
/// The whoami endpoint serves the metadata; the usage endpoint must echo the
/// same account_id/user_id (the app-server compares them field-by-field).
pub const POOL_IDENTITY: PoolIdentity = PoolIdentity {
    email: "pool@herdex.local",
    user_id: "herdex-virtual-pool-user",
    account_id: "herdex-virtual-pool-account",
};

pub struct PoolIdentity {
    pub email: &'static str,
    pub user_id: &'static str,
    pub account_id: &'static str,
}

async fn whoami() -> Response {
    log::info!("whoami <- PAT auth bootstrap");
    let body = serde_json::json!({
        "email": POOL_IDENTITY.email,
        "chatgpt_user_id": POOL_IDENTITY.user_id,
        "chatgpt_account_id": POOL_IDENTITY.account_id,
        "chatgpt_plan_type": "pro",
        "chatgpt_account_is_fedramp": false,
    });
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// Serves the CLI's account usage poll: zero-cost probe of every enabled
/// account; observations feed back into the pool.
/// Auth note: when the CLI's chatgpt_base_url points here, the poll carries
/// the CLI's own ChatGPT OAuth token instead of a herdex API key — accept any
/// bearer on this LAN-only route (exposes usage percentages only).
/// The CLI's statusline data source: the pool as ONE virtual account —
/// used% is the capacity-weighted mean of per-account usage (token-calibrated
/// weights), so it always lands in 0-100.
async fn codex_usage(State(app): State<AppHandle>, headers: HeaderMap) -> Response {
    let has_bearer = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("Bearer "))
        .unwrap_or(false);
    if !has_bearer {
        return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
    }
    log::info!("usage poll <- {}", headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("-"));
    codex_usage_pool(&app).await
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

    let session_key = headers
        .get("Session-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let candidates = match app.pool.select(&model, &session_key) {
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

    // full rate-limit header observability: log every limit-related response
    // header verbatim (read-only; passthrough unaffected)
    {
        let mut rl = String::new();
        for (k, v) in res.headers() {
            let name = k.as_str();
            if name.starts_with("x-codex")
                || name.contains("used-percent")
                || name.contains("window-minutes")
                || name.contains("reset-at")
                || name.contains("reset-after")
                || name.contains("rate-limit")
            {
                if let Ok(vs) = v.to_str() {
                    rl.push_str(&format!(" {}={}", name, vs));
                }
            }
        }
        if !rl.is_empty() {
            log::info!(
                "rl-headers {} <- {} (status {}):{}",
                model,
                acc.email,
                res.status().as_u16(),
                rl
            );
        }
    }

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
        // every successful response carries the main window's live usage —
        // feed it to the probe sequence (dedupe: only % changes stored), which
        // makes capacity calibration converge orders of magnitude faster.
        // NOTE: main `x-codex-*` family, not bengalfox (spark window) above.
        let main_pri = header_pct(res.headers(), "x-codex-primary-used-percent");
        let main_reset = reset_epoch(
            res.headers(),
            "x-codex-primary-reset-at",
            "x-codex-primary-reset-after-seconds",
        );
        if let (Some(p), Some(r)) = (main_pri, main_reset) {
            let plan = headers_get(res.headers(), "x-codex-plan-type").unwrap_or_default();
            app.store.add_probe(&acc.id, crate::store::now_secs(), p, r, &plan);
        }
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
    // The CLI's statusline also updates from turn response headers, which
    // would otherwise overwrite the poll's pool value with the single
    // serving account's usage — rewrite to the pool aggregate for coherence.
    if resp_headers.contains_key("x-codex-primary-used-percent") {
        if let Some(p) = pool_used_pct(app) {
            if let Ok(v) = axum::http::HeaderValue::from_str(&format!("{p:.1}")) {
                out.headers_mut()
                    .unwrap()
                    .insert("x-codex-primary-used-percent", v);
            }
        }
    }

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

fn headers_get(h: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    h.get(name)?.to_str().ok().map(|s| s.to_string())
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

/// Capacity-weighted pool usage: each account weighs by its calibrated
/// tokens-per-1%; accounts without calibration share the mean of known
/// weights (1.0 when nothing is calibrated yet). Shared by the usage poll
/// and the outbound header rewrite so both CLI data sources agree.
fn weighted_used(entries: &[(f64, Option<f64>)]) -> f64 {
    let known: Vec<f64> = entries.iter().filter_map(|(_, w)| *w).collect();
    let fallback = if known.is_empty() {
        1.0
    } else {
        known.iter().sum::<f64>() / known.len() as f64
    };
    let mut wsum = 0.0f64;
    let mut wpct = 0.0f64;
    for (pct, w) in entries {
        let w = w.unwrap_or(fallback);
        wsum += w;
        wpct += w * pct;
    }
    if wsum > 0.0 { wpct / wsum } else { 0.0 }
}

/// Pool-wide primary usage for the outbound header rewrite, sourced from the
/// pool's latest "default" observations (refreshed by the usage poll and the
/// per-request header observations). None when the pool has no data yet.
fn pool_used_pct(app: &App) -> Option<f64> {
    let snap = app.pool.snapshot();
    let mut entries: Vec<(f64, Option<f64>)> = Vec::new();
    for (acc, by_model) in &snap {
        if let Some(q) = by_model.get("default") {
            let w = app
                .store
                .calibration(acc)
                .ok()
                .flatten()
                .map(|c| c.tokens_per_pct);
            entries.push((q.primary_pct, w));
        }
    }
    if entries.is_empty() {
        None
    } else {
        Some(weighted_used(&entries))
    }
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

#[derive(Default)]
struct WsTurnCtx {
    model: Option<String>,
    turn_start: Option<std::time::Instant>,
}

/// Inspects one upstream WS frame (JSON mirror of the SSE event stream) and
/// records what the HTTP path gets for free: per-turn usage into request_log,
/// rate-limit windows into the pool + probe sequence, and in-stream error
/// frames as warnings (e.g. server_is_overloaded — invisible until now).
/// Parse failures are silently ignored; sniffing must never disturb the pipe.
fn sniff_upstream_event(
    app: &AppHandle,
    ctx: &Mutex<WsTurnCtx>,
    acc: &crate::store::Account,
    api_key: &str,
    text: &str,
    started: std::time::Instant,
) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        "response.completed" => {
            let usage = v.pointer("/response/usage");
            let inp = usage
                .and_then(|u| u.get("input_tokens"))
                .and_then(|x| x.as_i64())
                .unwrap_or(0);
            let cached = usage
                .and_then(|u| u.get("cached_input_tokens"))
                .and_then(|x| x.as_i64())
                .unwrap_or(0);
            let out = usage
                .and_then(|u| u.get("output_tokens"))
                .and_then(|x| x.as_i64())
                .unwrap_or(0);
            if inp == 0 && out == 0 {
                return;
            }
            let (model, latency) = {
                let c = ctx.lock().unwrap();
                let ms = c
                    .turn_start
                    .map(|t| t.elapsed().as_millis() as i64)
                    .unwrap_or_else(|| started.elapsed().as_millis() as i64);
                (c.model.clone().unwrap_or_default(), ms)
            };
            app.store.add_log(&crate::store::LogEntry {
                ts: crate::store::now_secs(),
                account_email: acc.email.clone(),
                api_key: api_key.to_string(),
                model: model.clone(),
                status: 200,
                latency_ms: latency,
                input_tokens: inp,
                cached_tokens: cached,
                output_tokens: out,
                error: String::new(),
            });
            let _ = ctx.lock().unwrap().turn_start.take();
        }
        "codex.rate_limits" => {
            let pct = v
                .pointer("/rate_limits/primary/used_percent")
                .and_then(|x| x.as_f64());
            let Some(pct) = pct else { return };
            let reset = v
                .pointer("/rate_limits/primary/reset_at")
                .and_then(|x| x.as_i64())
                .unwrap_or(0);
            let window_secs = v
                .pointer("/rate_limits/primary/window_minutes")
                .and_then(|x| x.as_i64())
                .unwrap_or(10080)
                * 60;
            let plan = v
                .get("plan_type")
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string();
            let model = ctx.lock().unwrap().model.clone().unwrap_or_default();
            app.pool.observe(
                &acc.id,
                &model,
                crate::pool::Quota {
                    primary_pct: pct,
                    secondary_pct: 0.0,
                    primary_reset_at: reset,
                    secondary_reset_at: 0,
                    primary_window_secs: window_secs,
                    secondary_window_secs: 0,
                    observed_at: crate::store::now_secs(),
                },
            );
            if reset > 0 {
                app.store
                    .add_probe(&acc.id, crate::store::now_secs(), pct, reset, &plan);
            }
        }
        "error" => {
            let code = v
                .get("code")
                .or_else(|| v.pointer("/error/code"))
                .and_then(|c| c.as_str())
                .unwrap_or("unknown");
            let msg = v
                .get("message")
                .or_else(|| v.pointer("/error/message"))
                .and_then(|m| m.as_str())
                .unwrap_or("");
            log::warn!(
                "ws-error frame: {} code={} via {}: {}",
                model_or_default(ctx),
                acc.email,
                code,
                msg
            );
        }
        _ => {}
    }
}

fn model_or_default(ctx: &Mutex<WsTurnCtx>) -> String {
    ctx.lock().unwrap().model.clone().unwrap_or_default()
}


async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(app): State<AppHandle>,
    headers: HeaderMap,
) -> Response {
    let api_key = match auth_check(&app, &headers) {
        Ok(k) => k,
        Err(res) => return res,
    };
    let session_key = headers
        .get("Session-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let candidates = app.pool.select("", &session_key).unwrap_or_default();
    let Some(acc) = candidates.into_iter().next() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no account available for websocket").into_response();
    };
    app.pool.pin(&session_key, &acc.id, "");
    ws.on_upgrade(move |sock| async move { pipe(app, sock, headers, acc, api_key).await })
}

async fn pipe(app: AppHandle, sock: axum::extract::ws::WebSocket, headers: HeaderMap, acc: crate::store::Account, api_key: String) {
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    // bookkeeping shared between the two directions
    let ctx: Arc<Mutex<WsTurnCtx>> = Arc::new(Mutex::new(WsTurnCtx::default()));
    let started = std::time::Instant::now();

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

    log::info!(
        "ws pipe open <- {} (model {:?})",
        acc.email,
        ctx.lock().unwrap().model
    );
    let c2u = {
        let ctx = ctx.clone();
        tokio::spawn(async move {
        while let Some(Ok(msg)) = cl_rx.next().await {
            let out = match msg {
                axum::extract::ws::Message::Text(t) => {
                    // sniff the turn request for the model name + turn start
                    if t.contains("\"model\"") {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                            if let Some(m) = v.get("model").and_then(|m| m.as_str()) {
                                let mut c = ctx.lock().unwrap();
                                c.model = Some(m.to_string());
                                c.turn_start = Some(std::time::Instant::now());
                            }
                        }
                    }
                    WsMessage::Text(t.to_string())
                }
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
                log::info!("ws pipe c2u send failed <- {}", ctx.lock().unwrap().model.clone().unwrap_or_default());
                break;
            }
        }
        })
    };
    let u2c = {
        let ctx = ctx.clone();
        let app = app.clone();
        let acc = acc.clone();
        let api_key = api_key.clone();
        tokio::spawn(async move {
        while let Some(Ok(msg)) = up_rx.next().await {
            let out = match msg {
                WsMessage::Text(t) => {
                    // sniff upstream event frames: usage accounting, rate-limit
                    // observations, in-stream errors — HTTP path gets these from
                    // headers/stream tail, WS would otherwise be a total blind spot
                    if t.starts_with('{') {
                        sniff_upstream_event(&app, &ctx, &acc, &api_key, &t, started);
                    }
                    axum::extract::ws::Message::Text(t.as_str().into())
                }
                WsMessage::Binary(b) => axum::extract::ws::Message::Binary(b.into()),
                WsMessage::Ping(b) => axum::extract::ws::Message::Ping(b.into()),
                WsMessage::Pong(b) => axum::extract::ws::Message::Pong(b.into()),
                WsMessage::Close(f) => {
                    log::info!(
                        "ws upstream close <- {} code={} reason={:?}",
                        ctx.lock().unwrap().model.clone().unwrap_or_default(),
                        f.as_ref().map(|c| u16::from(c.code)).unwrap_or(0),
                        f.as_ref().map(|c| c.reason.clone())
                    );
                    axum::extract::ws::Message::Close(f.map(|f| axum::extract::ws::CloseFrame {
                        code: u16::from(f.code),
                        reason: f.reason.to_string().as_str().into(),
                    }))
                }
                _ => continue,
            };
            if cl_tx.send(out).await.is_err() {
                log::info!("ws pipe u2c send failed <- {}", ctx.lock().unwrap().model.clone().unwrap_or_default());
                break;
            }
        }
        let _ = cl_tx.close().await;
    })
    };
    let _ = c2u.await;
    let _ = u2c.await;
    app.pool.mark_used(&acc.id);
}

use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Official upstream path for the apps MCP (see codex_apps_mcp_url_for_base_url:
/// with the default chatgpt_base_url it is {backend-api}/ps/mcp, not under
/// /api/codex).
const APPS_MCP_UPSTREAM: &str = "https://chatgpt.com/backend-api/ps/mcp";

/// Apps MCP proxy: re-authenticates the forwarded streamable-HTTP session with
/// pool-account credentials — the exact request shape the /responses flow
/// already uses, so chatgpt.com accepts it instead of challenging.
async fn codex_apps_mcp(
    State(app): State<AppHandle>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let snapshot = app.pool.snapshot();
    let pick = app
        .store
        .list_accounts()
        .unwrap_or_default()
        .into_iter()
        .filter(|a| !a.disabled)
        .min_by(|a, b| {
            let score = |a: &Account| {
                snapshot
                    .get(&a.id)
                    .and_then(|m| m.get("default"))
                    .map(|q| q.primary_pct.max(q.secondary_pct))
                    .unwrap_or(0.0)
            };
            score(a)
                .partial_cmp(&score(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    let Some(acc) = pick else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no enabled account").into_response();
    };

    let mut rb = app
        .http
        .request(method.clone(), APPS_MCP_UPSTREAM)
        .bearer_auth(&acc.access_token)
        .header("Chatgpt-Account-Id", &acc.account_id);
    for (k, v) in &app.cfg.header_defaults {
        rb = rb.header(k, v);
    }
    for name in ["Accept", "Content-Type", "Mcp-Session-Id", "Mcp-Protocol-Version", "Originator", "User-Agent"] {
        if let Some(v) = headers.get(name) {
            rb = rb.header(name, v);
        }
    }
    let res = match rb.body(body.to_vec()).send().await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("apps mcp proxy: {e}")).into_response();
        }
    };
    let status = res.status();
    log::info!("apps-mcp-proxy {} -> {}", method, status.as_u16());

    let mut out = Response::builder().status(axum::http::StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY));
    for (k, v) in res.headers() {
        if HOP_HEADERS.contains(&k.as_str()) || k == "content-length" {
            continue;
        }
        if let (Ok(name), Ok(val)) = (
            axum::http::HeaderName::from_bytes(k.as_str().as_bytes()),
            axum::http::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            out = out.header(name, val);
        }
    }
    out.body(Body::from_stream(res.bytes_stream()))
        .unwrap_or_else(|e| (StatusCode::BAD_GATEWAY, e.to_string()).into_response())
}

/// Catch-all reverse proxy to the real ChatGPT backend for paths herdex does
/// not own (accounts/check, credit tools, ...). The CLI's
/// chatgpt_base_url points here, so anything unhandled is forwarded verbatim
/// with the caller's own Authorization — restoring the direct-to-chatgpt
/// behavior those calls had before the redirect.
pub async fn codex_backend_fallback(
    State(app): State<AppHandle>,
    method: axum::http::Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    const UPSTREAM_ROOT: &str = "https://chatgpt.com/backend-api";
    let path_q = uri.path_and_query().map(|p| p.as_str()).unwrap_or(uri.path());
    // CodexApi path style (chatgpt_base_url without /backend-api) uses
    // /api/codex/*; the official ChatGPT backend serves the same endpoints
    // under /backend-api/wham/* — except the apps MCP, which lives directly
    // under /backend-api/ps/mcp (see codex_apps_mcp_url_for_base_url).
    let rel = if let Some(rest) = path_q.strip_prefix("/api/codex") {
        if rest.starts_with("/ps/") {
            rest.to_string()
        } else {
            format!("/wham{}", rest)
        }
    } else {
        path_q.strip_prefix("/backend-api").unwrap_or(path_q).to_string()
    };
    let upstream_path = format!("/backend-api{}", rel);
    let url = format!("{}{}", UPSTREAM_ROOT, rel);

    let mut rb = app.http.request(method.clone(), &url);
    for (k, v) in &headers {
        let name = k.as_str();
        if name == "host" || HOP_HEADERS.contains(&name) || name == "content-length" {
            continue;
        }
        rb = rb.header(name, v.as_bytes());
    }
    let res = match rb.body(body.to_vec()).send().await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("backend proxy: {e}")).into_response();
        }
    };
    let status = res.status();
    log::info!("backend-proxy {} {} -> {}", method, upstream_path, status.as_u16());

    let mut out = Response::builder().status(axum::http::StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY));
    for (k, v) in res.headers() {
        if HOP_HEADERS.contains(&k.as_str()) || k == "content-length" {
            continue;
        }
        if let (Ok(name), Ok(val)) = (
            axum::http::HeaderName::from_bytes(k.as_str().as_bytes()),
            axum::http::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            out = out.header(name, val);
        }
    }
    out.body(Body::from_stream(res.bytes_stream()))
        .unwrap_or_else(|e| (StatusCode::BAD_GATEWAY, e.to_string()).into_response())
}

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
