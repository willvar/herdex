//! Codex client-facing routes: pure pass-through router with pool-aware
//! auth swapping. Request bodies are forwarded byte-for-byte, so codex wire
//! features (reasoning, service tier, encrypted CoT) are inherited for free.

use crate::app::{App, AppHandle};
use crate::pool;
use crate::store::{Account, LogEntry, Store};
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
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

const MAX_PREFIX_BYTES: usize = 2 * 1024 * 1024;
// Observation is best-effort; exceeding this budget never truncates the wire.
const MAX_OBSERVATION_BYTES: usize = 8 * 1024 * 1024;

pub fn router() -> axum::Router<AppHandle> {
    axum::Router::new()
        .route("/v1/models", axum::routing::get(models))
        .route("/v1/responses", axum::routing::post(responses))
        .route(
            "/backend-api/codex/responses",
            axum::routing::post(responses),
        )
        // codex CLI's statusline is fed exclusively by its periodic account
        // usage poll (GET {base}/api/codex/usage for CodexApi-path base URLs);
        // without this route the poll 404s and the statusline freezes
        .route("/api/codex/usage", axum::routing::get(codex_usage))
        .route("/v1/api/codex/usage", axum::routing::get(codex_usage))
        .route("/wham/usage", axum::routing::get(codex_usage))
        .route(
            "/v1/user-auth-credential/whoami",
            axum::routing::get(whoami),
        )
        .route("/user-auth-credential/whoami", axum::routing::get(whoami))
        // apps MCP: forwarded with pool-account credentials (the same auth
        // shape /responses uses) because the CLI's own OAuth forwarded from
        // behind the gateway gets Cloudflare-challenged
        .route("/api/codex/ps/mcp", axum::routing::any(codex_apps_mcp))
        .route("/v1/api/codex/ps/mcp", axum::routing::any(codex_apps_mcp))
}

// Err carries an axum Response; boxing everywhere is not worth it
#[allow(clippy::result_large_err)]
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
            Err(e) => {
                log::warn!("usage poll: wham fetch failed for {}: {e}", a.email);
                continue;
            }
        };
        if let Ok(report) = crate::usage::parse(body.as_bytes()) {
            if !a.plan_type.is_empty()
                && !report.plan_type.is_empty()
                && report.plan_type != a.plan_type
            {
                log::info!(
                    "plan changed for {}: {} -> {}",
                    a.email,
                    a.plan_type,
                    report.plan_type
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
        &entries
            .iter()
            .map(|e| (e.pct, e.weight))
            .collect::<Vec<_>>(),
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
    log::info!(
        "usage poll <- {}",
        headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-")
    );
    codex_usage_pool(&app).await
}

/// Decides whether one account's failure should move to the next.
fn failoverable(status: u16, body: &str) -> bool {
    match status {
        // status 0 = connection-level failure (DNS/TCP/TLS/timeout) — never
        // the account's fault; always try the next candidate
        0 => true,
        401 | 403 | 408 | 429 | 500 | 502 | 503 | 504 => true,
        // plan-gated models reject per-account (e.g. spark on plus); try next
        400 => body.contains("not supported"),
        _ => false,
    }
}

fn upstream_status(last: u16) -> StatusCode {
    StatusCode::from_u16(if last >= 400 { last } else { 502 }).unwrap_or(StatusCode::BAD_GATEWAY)
}

async fn responses(State(app): State<AppHandle>, headers: HeaderMap, body: Bytes) -> Response {
    let api_key = match auth_check(&app, &headers) {
        Ok(k) => k,
        Err(res) => return res,
    };
    let probe: Result<serde_json::Value, _> = serde_json::from_slice(&body);
    let model = match probe
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from))
    {
        Some(m) => m,
        None => return (
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"missing \"model\" in request body","type":"server_error"}}"#,
        )
            .into_response(),
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
        match attempt_once(
            &app,
            &headers,
            &body,
            &acc,
            &model,
            start,
            &session_key,
            &api_key,
        )
        .await
        {
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

#[allow(clippy::too_many_arguments)] // the pipeline stages are all distinct inputs
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
    let url = format!(
        "{}/responses",
        app.cfg.upstream.base_url.trim_end_matches('/')
    );
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
    let mut res = loop {
        let rb = app
            .http
            .post(&url)
            .header(
                "Content-Type",
                client_headers
                    .get("Content-Type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/json"),
            )
            .header(
                "Accept",
                client_headers
                    .get("Accept")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("text/event-stream"),
            )
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
            let built = hr.body(text).map_err(|e| (500, e.to_string()))?;
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
    let pri_reset = reset_epoch(
        res.headers(),
        "x-codex-bengalfox-primary-reset-at",
        "x-codex-bengalfox-primary-reset-after-seconds",
    );
    let sec_reset = reset_epoch(
        res.headers(),
        "x-codex-bengalfox-secondary-reset-at",
        "x-codex-bengalfox-secondary-reset-after-seconds",
    );
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
        // feed it to the probe sequence, deduplicating only when quota,
        // metadata and the completed-log watermark are all unchanged.
        // NOTE: main `x-codex-*` family, not bengalfox (spark window) above.
        let main_pri = header_pct(res.headers(), "x-codex-primary-used-percent");
        let main_reset = reset_epoch(
            res.headers(),
            "x-codex-primary-reset-at",
            "x-codex-primary-reset-after-seconds",
        );
        if let (Some(p), Some(r)) = (main_pri, main_reset) {
            let plan = headers_get(res.headers(), "x-codex-plan-type").unwrap_or_default();
            app.store
                .add_probe(&acc.id, crate::store::now_secs(), p, r, &plan);
        }
    }

    let status = res.status().as_u16();
    let resp_headers = res.headers().clone();
    if status >= 400 {
        let snippet = res.text().await.unwrap_or_default();
        if failoverable(status, &snippet) {
            app.pool.mark_failure(&acc.id, model);
            log::info!(
                "{model} -> {status} ({}ms) via {}: {}",
                start.elapsed().as_millis(),
                acc.email,
                truncate(&snippet)
            );
            return Err((status, format!("{}: {status} {snippet}", acc.email)));
        }
        // genuine client error: pass through untouched
        let mut out = Response::builder()
            .status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST));
        copy_headers(out.headers_mut().unwrap(), &resp_headers);
        log::info!(
            "{model} -> {status} ({}ms) via {}",
            start.elapsed().as_millis(),
            acc.email
        );
        return out
            .body(Body::from(snippet))
            .map_err(|e| (500, e.to_string()));
    }

    // success status, BUT the upstream may carry in-stream error events
    // inside 200 streams (chatgpt.com sends server_is_overloaded/slow_down
    // after only lifecycle events). Buffer upstream chunks until the first
    // CONTENT event: an error arriving before any content is still
    // failoverable — nothing user-visible has reached the client. Progress
    // markers are the content-bearing events (output items, deltas); the
    // bare lifecycle frames (created/in_progress) carry nothing.
    let mut buffered = Vec::new();
    let mut remainder = Bytes::new();
    // Construct the guard before any await or response-body polling: dropping
    // either the handler or the unpolled body must still finalize this attempt.
    let mut observed = StreamLog {
        stats: StreamStats::new(
            resp_headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
        ),
        store: app.store.clone(),
        pool: app.pool.clone(),
        account_id: acc.id.clone(),
        entry: Some(LogEntry {
            account_id: acc.id.clone(),
            account_email: acc.email.clone(),
            api_key: api_key.to_owned(),
            model: model.to_owned(),
            status: status as i64,
            latency_ms: latency,
            ..Default::default()
        }),
    };
    loop {
        let ended = match res.chunk().await {
            Ok(Some(c)) => {
                // Apply the budget to bytes, not transport chunks. Observe
                // only the bytes within it before deciding whether to commit.
                let take = c.len().min(MAX_PREFIX_BYTES - buffered.len());
                observed.stats.push(&c[..take]);
                buffered.extend_from_slice(&c[..take]);
                remainder = c.slice(take..);
                false
            }
            Ok(None) => {
                observed.eof();
                true
            }
            Err(_) => {
                observed.transport_error();
                return Err((0, format!("{}: upstream stream error", acc.email)));
            }
        };
        // The same observer spans every chunk, including an incomplete
        // event carried over into the streaming phase. Event order decides
        // whether a coalesced content/error pair can still fail over.
        if let Some(code) = observed.stats.error_before_content.clone() {
            observed.record(None, false);
            log::warn!(
                "{} upstream error event before any content (code {:?}) via {} — failing over",
                model,
                code,
                acc.email
            );
            return Err((503, format!("{}: {code}", acc.email)));
        }
        if ended
            || observed.stats.content_seen
            || (!remainder.is_empty() && observed.stats.json_body.is_some())
        {
            // JSON is not subject to SSE event gating: large JSON bodies
            // retain the existing passthrough behavior and EOF usage parsing.
            observed.stats.push(&remainder);
            break;
        }
        if !remainder.is_empty() {
            // This is a local buffering policy, not an account failure. Return
            // directly so the caller neither retries nor cools this account.
            if let Some(entry) = observed.entry.as_mut() {
                entry.status = StatusCode::BAD_GATEWAY.as_u16() as i64;
            }
            observed.record(Some("prefix_limit_exceeded"), false);
            return Ok((
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({"error": {
                    "code": "prefix_limit_exceeded",
                    "message": "upstream SSE prefix exceeded the buffering limit before content"
                }})),
            )
                .into_response());
        }
    }
    // success: stream to client, log usage after the stream completes
    app.pool.mark_used(&acc.id);
    app.pool.pin(session_key, &acc.id, model);
    let mut out =
        Response::builder().status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK));
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

    let stream = async_stream::stream! {
        let mut res = res;
        if !buffered.is_empty() {
            yield Ok::<_, std::io::Error>(Bytes::from(buffered));
        }
        if !remainder.is_empty() {
            yield Ok::<_, std::io::Error>(remainder);
        }
        while let Some(chunk) = res.chunk().await.transpose() {
            match chunk {
                Ok(c) => {
                    observed.stats.push(&c);
                    yield Ok::<_, std::io::Error>(c);
                }
                Err(e) => {
                    // A body consumer may stop polling as soon as it receives
                    // Err, so finalize the upstream failure before yielding it.
                    observed.transport_error();
                    yield Err(std::io::Error::other(e));
                    return;
                }
            }
        }
        observed.eof();
    };
    out.body(Body::from_stream(stream))
        .map_err(|e| (500, e.to_string()))
}

/// Finalize once even when a response is dropped before its first poll or
/// while suspended at a yield. User cancellation alone never cools an account.
struct StreamLog {
    stats: StreamStats,
    store: Store,
    pool: pool::Pool,
    account_id: String,
    entry: Option<LogEntry>,
}

impl StreamLog {
    fn eof(&mut self) {
        self.stats.finish();
        self.record(None, false);
    }

    fn transport_error(&mut self) {
        self.record(Some("upstream_stream_error"), true);
    }

    fn record(&mut self, fallback_error: Option<&str>, upstream_failed: bool) {
        let Some(mut entry) = self.entry.take() else {
            return;
        };
        entry.ts = crate::store::now_secs();
        entry.input_tokens = self.stats.input;
        entry.cached_tokens = self.stats.cached;
        entry.output_tokens = self.stats.output;
        entry.error = self
            .stats
            .error
            .as_deref()
            .or(fallback_error)
            .or_else(|| {
                self.stats
                    .observation_limited
                    .then_some("observer_limit_exceeded")
            })
            .unwrap_or_default()
            .to_owned();
        if upstream_failed || self.stats.error.is_some() {
            self.pool.mark_failure(&self.account_id, &entry.model);
            log::warn!(
                "{} stream failed via {}: {}",
                entry.model,
                entry.account_email,
                entry.error
            );
        }
        self.store.add_log(&entry);
    }
}

impl Drop for StreamLog {
    fn drop(&mut self) {
        let error = (!self.stats.completed).then_some("client_cancelled");
        self.record(error, false);
    }
}

/// True when the chunk carries user-visible content: output items, text
/// deltas, function calls. Lifecycle frames (created/in_progress) do NOT
/// count — a turn can sit in "in_progress" for a long reasoning stretch
/// before its first content event, and that whole window is still
/// failoverable because the client has received nothing meaningful.
fn is_content_type(ty: &str) -> bool {
    matches!(
        ty,
        "response.output_item.added"
            | "response.output_text.delta"
            | "response.output_item.done"
            | "response.function_call_arguments.delta"
    )
}

fn header_pct(h: &reqwest::header::HeaderMap, name: &str) -> Option<f64> {
    h.get(name)?
        .to_str()
        .ok()?
        .trim_end_matches('%')
        .parse()
        .ok()
}

fn header_minutes(h: &reqwest::header::HeaderMap, name: &str) -> Option<i64> {
    let m: i64 = h.get(name)?.to_str().ok()?.parse().ok()?;
    (m > 0).then(|| m * 60)
}

/// Observe SSE or JSON without altering the bytes sent to the client. For
/// SSE only the unfinished line/event is retained; completed events become a small
/// summary, so a large response.completed never loses its usage to tail
/// truncation. Byte buffers also allow UTF-8 and delimiters to span chunks.
/// Non-streaming JSON responses are parsed as a single document at EOF.
/// Oversized documents/events are not inspected; SSE resumes at the next event.
#[derive(Default)]
struct StreamStats {
    json_body: Option<Vec<u8>>,
    line: Vec<u8>,
    data: Vec<u8>,
    line_seen: bool,
    after_cr: bool,
    line_nonempty: bool,
    skipping_event: bool,
    observation_limited: bool,
    content_seen: bool,
    completed: bool,
    error_before_content: Option<String>,
    error: Option<String>,
    input: i64,
    cached: i64,
    output: i64,
}

impl StreamStats {
    fn new(content_type: &str) -> Self {
        let media_type = content_type.split(';').next().unwrap_or("").trim();
        let is_json = media_type.eq_ignore_ascii_case("application/json")
            || media_type.to_ascii_lowercase().ends_with("+json");
        Self {
            json_body: is_json.then(Vec::new),
            ..Self::default()
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if let Some(body) = self.json_body.as_mut() {
            if !self.observation_limited {
                if chunk.len() > MAX_OBSERVATION_BYTES - body.len() {
                    self.observation_limited = true;
                    *body = Vec::new();
                } else {
                    body.extend_from_slice(chunk);
                }
            }
            return;
        }
        for part in chunk.split_inclusive(|b| *b == b'\r' || *b == b'\n') {
            if self.after_cr {
                self.after_cr = false;
                if part == b"\n" {
                    continue; // CRLF is one line ending, even across chunks
                }
            }
            let last = *part.last().unwrap();
            let ends_line = last == b'\r' || last == b'\n';
            let bytes = if ends_line {
                &part[..part.len() - 1]
            } else {
                part
            };
            self.line_nonempty |= !bytes.is_empty();
            if !self.skipping_event {
                if bytes.len() > MAX_OBSERVATION_BYTES - self.line.len() - self.data.len() {
                    self.observation_limited = true;
                    self.skipping_event = true;
                    self.line = Vec::new();
                    self.data = Vec::new();
                } else {
                    self.line.extend_from_slice(bytes);
                }
            }
            if ends_line {
                if self.skipping_event {
                    // Do not interpret a suffix of an oversized event. Resume
                    // only after its blank separator, even across chunk cuts.
                    self.skipping_event = self.line_nonempty;
                    self.line_seen = true;
                } else {
                    self.finish_line();
                }
                self.line_nonempty = false;
                self.after_cr = last == b'\r';
            }
        }
    }

    fn finish_line(&mut self) {
        // SSE permits one UTF-8 BOM at the start of the stream. Waiting for
        // the first full line naturally handles a BOM split across chunks.
        let line = if self.line_seen {
            self.line.as_slice()
        } else {
            self.line_seen = true;
            self.line
                .strip_prefix(b"\xef\xbb\xbf")
                .unwrap_or(&self.line)
        };
        if line.is_empty() {
            self.finish_event();
        } else if let Some(value) = line.strip_prefix(b"data:") {
            // SSE permits one optional space after the colon; multiple
            // data fields are joined with newlines before JSON decoding.
            self.data
                .extend_from_slice(value.strip_prefix(b" ").unwrap_or(value));
            self.data.push(b'\n');
        }
        self.line.clear();
    }

    fn finish_event(&mut self) {
        if let Ok(event) = serde_json::from_slice(&self.data) {
            self.observe(event);
        }
        self.data.clear();
    }

    // Some upstreams omit the last blank line. Accept a complete final
    // JSON event at EOF, but never treat partial JSON as content or usage.
    fn finish(&mut self) {
        if self.observation_limited && self.json_body.is_some() || self.skipping_event {
            return;
        }
        if let Some(body) = self.json_body.as_mut() {
            let body = std::mem::take(body);
            if let Ok(event) = serde_json::from_slice(&body) {
                self.observe(event);
            }
            return;
        }
        if !self.line.is_empty() {
            self.finish_line();
        }
        self.finish_event();
    }

    fn observe(&mut self, v: serde_json::Value) {
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if ty == "response.completed" {
            self.completed = true;
        }
        if is_content_type(ty) {
            self.content_seen = true;
        } else if ty == "error" {
            let code = v
                .get("code")
                .and_then(|c| c.as_str())
                .unwrap_or("unknown")
                .to_owned();
            if !self.content_seen && self.error_before_content.is_none() {
                self.error_before_content = Some(code.clone());
            }
            self.error = Some(code);
        }
        // response.completed nests usage under response; token_count
        // events carry it at the top level.
        let Some(u) = v
            .get("response")
            .and_then(|r| r.get("usage"))
            .or_else(|| v.get("usage"))
        else {
            return;
        };
        if let Some(n) = u.get("input_tokens").and_then(|x| x.as_i64()) {
            self.input = n;
        }
        if let Some(n) = u
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .or_else(|| u.get("cached_tokens"))
            .and_then(|x| x.as_i64())
        {
            self.cached = n;
        }
        if let Some(n) = u.get("output_tokens").and_then(|x| x.as_i64()) {
            self.output = n;
        }
    }
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
    if wsum > 0.0 {
        wpct / wsum
    } else {
        0.0
    }
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
    for marker in [
        "Unsupported parameter: ",
        "Unrecognized request argument supplied: ",
    ] {
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
    for name in [
        "Accept",
        "Content-Type",
        "Mcp-Session-Id",
        "Mcp-Protocol-Version",
        "Originator",
        "User-Agent",
    ] {
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

    let mut out = Response::builder().status(
        axum::http::StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
    );
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
    let path_q = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or(uri.path());
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
        path_q
            .strip_prefix("/backend-api")
            .unwrap_or(path_q)
            .to_string()
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
    log::info!(
        "backend-proxy {} {} -> {}",
        method,
        upstream_path,
        status.as_u16()
    );

    let mut out = Response::builder().status(
        axum::http::StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
    );
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
    use futures_util::StreamExt;

    #[tokio::test]
    async fn dropped_stream_finalizes_once_before_poll_and_after_yield() {
        for poll_first in [false, true] {
            for outcome in ["cancelled", "completed", "eof", "transport_error"] {
                let dir = std::env::temp_dir()
                    .join(format!("herdex-stream-finalize-{}", uuid::Uuid::new_v4()));
                let store = Store::open(dir.to_str().unwrap()).unwrap();
                for id in ["a1", "a2"] {
                    store
                        .upsert_account(&Account {
                            id: id.into(),
                            email: format!("{id}@test"),
                            plan_type: "pro".into(),
                            ..Default::default()
                        })
                        .unwrap();
                }
                let pool = pool::Pool::new(store.clone());
                let mut observed = StreamLog {
                    stats: StreamStats::default(),
                    store: store.clone(),
                    pool: pool.clone(),
                    account_id: "a1".into(),
                    entry: Some(LogEntry {
                        account_email: "a1@test".into(),
                        model: "gpt-5.5".into(),
                        status: 200,
                        ..Default::default()
                    }),
                };
                observed.stats.push(b"data:{\"type\":\"token_count\",\"usage\":{\"input_tokens\":120,\"cached_tokens\":80,\"output_tokens\":45}}\n\n");
                match outcome {
                    "completed" => observed
                        .stats
                        .push(b"data:{\"type\":\"response.completed\"}\n\n"),
                    "eof" => observed.eof(),
                    "transport_error" => observed.transport_error(),
                    _ => {}
                }
                // Check both an unpolled body and one suspended at yield. The
                // guard must already exist before the generator starts running.
                let body = Body::from_stream(async_stream::stream! {
                    yield Ok::<_, std::io::Error>(Bytes::from_static(b"buffered prefix"));
                    observed.eof();
                });
                let mut stream = body.into_data_stream();
                if poll_first {
                    assert_eq!(stream.next().await.unwrap().unwrap(), "buffered prefix");
                }
                drop(stream);
                let logs = store.recent_logs(5).unwrap();
                assert_eq!(logs.len(), 1, "{outcome}: no missing or duplicate log");
                assert_eq!(
                    (
                        logs[0].input_tokens,
                        logs[0].cached_tokens,
                        logs[0].output_tokens
                    ),
                    (120, 80, 45)
                );
                assert_eq!(
                    logs[0].error,
                    match outcome {
                        "cancelled" => "client_cancelled",
                        "transport_error" => "upstream_stream_error",
                        _ => "",
                    }
                );
                assert_eq!(
                    pool.select("gpt-5.5", "").unwrap().len(),
                    if outcome == "transport_error" { 1 } else { 2 }
                );
                drop(pool);
                drop(store);
                std::fs::remove_dir_all(dir).unwrap();
            }
        }
    }

    #[test]
    fn rejected_param_extracts_named_params() {
        assert_eq!(
            rejected_param(r#"{"detail":"Unsupported parameter: max_output_tokens"}"#),
            Some("max_output_tokens".to_string())
        );
        assert_eq!(
            rejected_param(
                r#"{"error":{"message":"Unrecognized request argument supplied: stream_options"}}"#
            ),
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
        let out: serde_json::Value =
            serde_json::from_slice(&remove_param(body, "reasoning.effort").unwrap()).unwrap();
        assert!(out["reasoning"].get("effort").is_none());
        assert!(remove_param(body, "missing.path").is_none());
        assert!(remove_param(b"not json", "x").is_none());
    }

    #[test]
    fn weighted_pool_aggregation_uses_calibration_weights() {
        // Pro 10x capacity at 29%, three prolite at 30% — the pool must be
        // dominated by Pro's weight, NOT an equal-weight mean (29.75)
        let entries = vec![
            (29.0, Some(10_070_419.8)),
            (30.0, Some(1_937_314.0)),
            (30.0, Some(2_215_704.4)),
            (30.0, Some(1_950_328.3)),
        ];
        let used = weighted_used(&entries);
        // 29x10.07M + 30x(1.94+2.22+1.95)M over 16.17M total
        assert!((used - 29.377).abs() < 0.05, "got {used}");

        // single entry degenerates to its own value
        assert_eq!(weighted_used(&[(29.0, Some(1e6))]), 29.0);

        // nothing calibrated → equal weights
        let e = vec![(29.0, None::<f64>), (30.0, None)];
        assert!((weighted_used(&e) - 29.5).abs() < 1e-9);
    }

    #[test]
    fn stream_error_detection() {
        let error = b"data:{\"type\": \"error\", \"code\": \"slow_down\"}\n\n";
        let content = b"data: {\"type\":\"response.output_item.added\"}\n\n";
        let mut stats = StreamStats::default();
        stats.push(error);
        stats.push(content);
        assert!(stats.content_seen);
        assert_eq!(stats.error_before_content.as_deref(), Some("slow_down"));
        assert_eq!(stats.error.as_deref(), Some("slow_down"));

        let mut stats = StreamStats::default();
        stats.push(content);
        stats.push(error);
        assert!(stats.content_seen);
        assert!(stats.error_before_content.is_none());
        assert_eq!(stats.error.as_deref(), Some("slow_down"));
    }

    #[test]
    fn stream_events_survive_every_byte_split() {
        // Multiline data, optional spaces, all SSE line endings, and a
        // multibyte character must behave identically at every split.
        let wire = concat!(
            ": heartbeat\r\n\r\n",
            "data:{\"type\": \"response.output_text.delta\",\r\n",
            "data: \"delta\": \"你好\"}\r\n\r\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":",
            "{\"input_tokens\":120,\"input_tokens_details\":{\"cached_tokens\":80},\"output_tokens\":45}}}\r\r",
            "data: [DONE]\n\n"
        ).as_bytes();
        for split in 0..=wire.len() {
            let mut stats = StreamStats::default();
            stats.push(&wire[..split]);
            stats.push(&wire[split..]);
            assert!(stats.content_seen, "split at {split}");
            assert!(stats.error.is_none(), "split at {split}");
            assert_eq!(
                (stats.input, stats.cached, stats.output),
                (120, 80, 45),
                "split at {split}"
            );
        }
        let mut stats = StreamStats::default();
        for byte in wire.chunks(1) {
            stats.push(byte);
        }
        assert!(stats.content_seen);
        assert_eq!((stats.input, stats.cached, stats.output), (120, 80, 45));
    }

    #[test]
    fn stream_accepts_bom_only_at_start_across_chunks() {
        for (event, is_content) in [
            (
                "{\"type\":\"response.output_text.delta\",\"delta\":\"你好\"}",
                true,
            ),
            ("{\"type\":\"error\",\"code\":\"slow_down\"}", false),
        ] {
            let wire = format!("\u{feff}data: {event}\n\n");
            for size in 1..=wire.len() {
                let mut stats = StreamStats::default();
                for chunk in wire.as_bytes().chunks(size) {
                    stats.push(chunk);
                }
                assert_eq!(stats.content_seen, is_content, "chunk size {size}");
                assert_eq!(
                    stats.error_before_content.as_deref(),
                    if is_content { None } else { Some("slow_down") },
                    "chunk size {size}"
                );
            }
        }
        let mut stats = StreamStats::default();
        stats.push(b": heartbeat\n\n");
        stats.push("\u{feff}data: {\"type\":\"error\",\"code\":\"slow_down\"}\n\n".as_bytes());
        assert!(
            stats.error.is_none(),
            "a later BOM is not a data field prefix"
        );
    }

    #[test]
    fn json_responses_preserve_usage_across_chunks() {
        let body = br#"{
            "object": "response",
            "usage": {
                "input_tokens": 120,
                "input_tokens_details": {"cached_tokens": 80},
                "output_tokens": 45
            }
        }"#;
        for content_type in [
            "application/json; charset=utf-8",
            "Application/JSON",
            "application/response+json",
        ] {
            let mut stats = StreamStats::new(content_type);
            for byte in body.chunks(1) {
                stats.push(byte);
            }
            assert_eq!(stats.input, 0, "JSON is observed only at EOF");
            stats.finish();
            assert_eq!((stats.input, stats.cached, stats.output), (120, 80, 45));
            stats.finish();
            assert_eq!((stats.input, stats.cached, stats.output), (120, 80, 45));
            assert!(stats.json_body.as_ref().unwrap().is_empty());
        }
    }

    #[test]
    fn stream_waits_for_event_boundary_and_accepts_final_usage_at_eof() {
        let mut stats = StreamStats::default();
        stats.push(b"data: {\"type\":\"response.output_item.added\"}\n");
        assert!(!stats.content_seen);
        stats.push(b"\n");
        assert!(stats.content_seen);
        stats.push(b"data: {\"type\":\"response.completed\",\"usage\":{\"input_tokens\":7,\"cached_tokens\":2,\"output_tokens\":3}}");
        stats.finish();
        assert_eq!((stats.input, stats.cached, stats.output), (7, 2, 3));

        let mut torn = StreamStats::default();
        torn.push(b"data: {\"type\":\"err");
        torn.finish();
        assert!(torn.error.is_none());
        assert!(!torn.content_seen);
    }

    #[test]
    fn network_failures_failover() {
        // status 0 (DNS/TCP/TLS/timeout) is never the account's fault
        assert!(failoverable(0, ""));
        // ...and never mapped to a meaningful upstream status for the client
        assert_eq!(upstream_status(0), StatusCode::BAD_GATEWAY);
    }
}
