//! Codex client-facing routes: pure pass-through router with pool-aware
//! auth swapping. Request bodies are forwarded byte-for-byte, so codex wire
//! features (reasoning, service tier, encrypted CoT) are inherited for free.

use crate::app::{App, AppHandle};
use crate::pool;
use crate::store::{Account, LogEntry, RequestDiagnostics, Store};
use crate::usage::{Limit, Window};
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use std::collections::BTreeMap;
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
        // model discovery: codex CLI reads {chatgpt_base_url}/models, other
        // clients read /v1/models — both serve the enabled-account union;
        // per-request routing filters by each account's catalog
        .route("/models", axum::routing::get(models))
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
        .route(
            "/api/codex/accounts/check",
            axum::routing::get(accounts_check),
        )
        .route(
            "/backend-api/wham/accounts/check",
            axum::routing::get(accounts_check),
        )
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

/// Model discovery: the union of existing, enabled accounts' discovered
/// catalogs (or the config override). Requests skip accounts whose known
/// catalogs lack the requested model.
async fn models(State(app): State<AppHandle>, headers: HeaderMap) -> Response {
    if auth_check(&app, &headers).is_err() {
        return (StatusCode::UNAUTHORIZED, "invalid api key").into_response();
    }
    if app.model_version().is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Codex release version unavailable",
        )
            .into_response();
    }
    // priority: config override > upstream-discovered union
    // No hardcoded fallback: return an empty list when no enabled account
    // has a known catalog. The background loop collects catalogs on startup
    // and each cycle.
    let slugs: Vec<String> = if app.cfg.models.is_empty() {
        match app.pool.union_models() {
            Ok(models) => models,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        }
    } else {
        app.cfg.models.clone()
    };
    let data: Vec<serde_json::Value> = slugs
        .iter()
        .map(|m| serde_json::json!({"id": m, "object": "model", "owned_by": "openai"}))
        .collect();
    axum::Json(serde_json::json!({"object": "list", "data": data})).into_response()
}

#[derive(Clone, Copy)]
struct PoolUsageWindow {
    seconds: i64,
    pct: f64,
    reset_at: Option<i64>,
}

impl PoolUsageWindow {
    fn from_window(window: &Window) -> Option<Self> {
        let pct = window.used_pct.filter(|p| p.is_finite())?;
        let seconds = window
            .window_seconds
            .filter(|s| *s > 0 && i32::try_from(*s).is_ok())?;
        Some(Self {
            seconds,
            pct: pct.clamp(0.0, 100.0),
            reset_at: window
                .reset_at
                .filter(|at| *at > 0 && i32::try_from(*at).is_ok()),
        })
    }

    fn payload(self, now: i64) -> serde_json::Value {
        // Codex deserializes all four fields as i32, including percentages.
        serde_json::json!({
            "used_percent": self.pct.round() as i64,
            "limit_window_seconds": self.seconds,
            "reset_at": self.reset_at.unwrap_or(0),
            "reset_after_seconds": self.reset_at.map(|at| (at - now).clamp(0, i32::MAX as i64)).unwrap_or(0),
        })
    }
}

/// Group by the actual duration, not by upstream primary/secondary position.
/// Historical calibration describes the primary window only; secondary
/// windows use the existing unknown-capacity fallback within their own group.
fn aggregate_usage_windows(entries: &[(Limit, Option<f64>)]) -> Vec<PoolUsageWindow> {
    let mut groups: BTreeMap<i64, Vec<(PoolUsageWindow, Option<f64>)>> = BTreeMap::new();
    for (limit, weight) in entries {
        let mut primary = PoolUsageWindow::from_window(&limit.primary);
        let mut secondary = PoolUsageWindow::from_window(&limit.secondary);
        if let (Some(p), Some(s)) = (primary, secondary) {
            if p.seconds == s.seconds {
                // One account contributes once per duration, retaining the
                // tighter observation if both slots describe the same period.
                primary = Some(if s.pct > p.pct { s } else { p });
                secondary = None;
            }
        }
        for (window, weight) in [(primary, *weight), (secondary, None)] {
            if let Some(window) = window {
                groups
                    .entry(window.seconds)
                    .or_default()
                    .push((window, weight));
            }
        }
    }
    groups
        .into_iter()
        .map(|(seconds, samples)| PoolUsageWindow {
            seconds,
            pct: weighted_used(
                &samples
                    .iter()
                    .map(|(w, weight)| (w.pct, *weight))
                    .collect::<Vec<_>>(),
            ),
            reset_at: samples.iter().filter_map(|(w, _)| w.reset_at).min(),
        })
        .collect()
}

fn pool_usage_payload(windows: &[PoolUsageWindow], allowed: bool, now: i64) -> serde_json::Value {
    serde_json::json!({
        // This is the virtual workspace's plan, matching whoami/accounts/check,
        // not whichever real account currently has the lowest used percentage.
        "plan_type": "pro",
        "rate_limit": {
            "allowed": allowed,
            "limit_reached": !allowed,
            "primary_window": windows.first().map(|w| w.payload(now)),
            "secondary_window": windows.get(1).map(|w| w.payload(now)),
        },
        // Preserve further durations without mixing or silently dropping them.
        "additional_rate_limits": windows.iter().skip(2).map(|w| serde_json::json!({
            "limit_name": "herdex pool",
            "metered_feature": format!("herdex_pool_{}", w.seconds),
            "rate_limit": {
                "allowed": w.pct < 100.0,
                "limit_reached": w.pct >= 100.0,
                "primary_window": w.payload(now),
            },
        })).collect::<Vec<_>>(),
        "rate_limit_reset_credits": { "available_count": 0 },
        "account_id": POOL_IDENTITY.account_id,
        "user_id": POOL_IDENTITY.user_id,
    })
}

/// Probe enabled accounts and aggregate each duration independently. Percentages,
/// calibration weights and the earliest reset all belong to the same group.
async fn codex_usage_pool(app: &App) -> Response {
    let accounts = match app.store.list_accounts() {
        Ok(accounts) => accounts,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
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

    let mut entries = Vec::new();
    for (a, rep) in &collected {
        let weight = app
            .store
            .calibration(&a.id)
            .ok()
            .flatten()
            .map(|c| c.tokens_per_pct);
        entries.push((rep.main.clone(), weight));
    }
    let windows = aggregate_usage_windows(&entries);
    if windows.is_empty() {
        return (StatusCode::BAD_GATEWAY, "no known account usage window").into_response();
    }
    let allowed = entries.iter().any(|(limit, _)| {
        limit.allowed
            && !limit.limit_reached
            && (PoolUsageWindow::from_window(&limit.primary).is_some()
                || PoolUsageWindow::from_window(&limit.secondary).is_some())
    });
    let payload = pool_usage_payload(&windows, allowed, crate::store::now_secs());
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

/// The pool is one virtual workspace, matching the PAT whoami identity.
/// Codex resolves NO_CONSTRAINT to its configured HTTPS bootstrap origin,
/// keeping gateway credentials and model traffic on this gateway.
async fn accounts_check(State(app): State<AppHandle>, headers: HeaderMap) -> Response {
    if let Err(response) = auth_check(&app, &headers) {
        return response;
    }
    axum::Json(serde_json::json!({
        "accounts": [{
            "id": POOL_IDENTITY.account_id,
            "name": "herdex",
            "plan_type": "pro",
            "structure": "personal",
            "workspace_backend_origin": "NO_CONSTRAINT",
            "account_routing_override": "NO_CONSTRAINT",
        }],
        "account_ordering": [POOL_IDENTITY.account_id],
        "default_account_id": POOL_IDENTITY.account_id,
    }))
    .into_response()
}

/// Serves the CLI's account usage poll: zero-cost probe of every enabled
/// account; observations feed back into the pool.
/// Auth note: when the CLI's chatgpt_base_url points here, the poll carries
/// the CLI's own ChatGPT OAuth token instead of a herdex API key — accept any
/// bearer on this LAN-only route (exposes usage percentages only).
/// The CLI's statusline data source: one virtual account, with independently
/// aggregated windows for each actual duration.
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
    let (model, service_tier) = match probe.ok().and_then(|v| {
        let model = v.get("model")?.as_str()?.to_owned();
        let tier = match v.get("service_tier") {
            None | Some(serde_json::Value::Null) => Some(String::new()),
            Some(serde_json::Value::String(tier)) => Some(tier.clone()),
            _ => None,
        };
        Some((model, tier))
    }) {
        Some(metadata) => metadata,
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
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut request_log = RequestLog {
        entry: LogEntry {
            api_key,
            model: model.clone(),
            service_tier,
            diagnostics: Some(RequestDiagnostics {
                request_id: request_id.clone(),
                ..Default::default()
            }),
            ..Default::default()
        },
        started: Instant::now(),
        attempts: 0,
    };
    for attempt in 0..attempts {
        let mut acc = candidates[attempt % candidates.len()].clone();
        let mut observed = request_log.begin(&app, &acc);
        if (acc.expires_at - crate::store::now_secs()) < 300 {
            observed.diagnostics().stage = "refresh".into();
            if app.refresh_expired(&mut acc).await.is_err() {
                observed.record(Some("token_refresh_failed"), true);
                last_status = 401;
                last_err = format!("token refresh failed for {}", acc.email);
                continue;
            }
        }
        match attempt_once(
            &app,
            &headers,
            &body,
            &acc,
            &session_key,
            &mut request_log,
            observed,
        )
        .await
        {
            Ok(mut resp) => {
                resp.headers_mut()
                    .insert("x-herdex-request-id", request_id.parse().unwrap());
                return resp;
            }
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
    let mut response = (upstream_status(last_status), axum::Json(body)).into_response();
    response
        .headers_mut()
        .insert("x-herdex-request-id", request_id.parse().unwrap());
    response
}

type AttemptResult = Result<Response, (u16, String)>;

async fn attempt_once(
    app: &App,
    client_headers: &HeaderMap,
    body: &[u8],
    acc: &Account,
    session_key: &str,
    request_log: &mut RequestLog,
    mut observed: StreamLog,
) -> AttemptResult {
    let model = request_log.entry.model.clone();
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
        observed.diagnostics().stage = "request_headers".into();
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
        let mut res = match res {
            Ok(r) => r,
            Err(e) => {
                if e.is_connect() {
                    observed.diagnostics().stage = "connect".into();
                }
                observed.diagnostics().error_detail = transport_detail(e);
                observed.record(Some("upstream_request_error"), true);
                return Err((0, format!("{}: upstream request error", acc.email)));
            }
        };
        let status = res.status().as_u16();
        let entry = observed.entry.as_mut().unwrap();
        entry.status = status as i64;
        entry.latency_ms = request_log.started.elapsed().as_millis() as i64;
        observed.diagnostics().upstream_request_id = ["x-request-id", "openai-request-id"]
            .into_iter()
            .filter_map(|name| res.headers().get(name)?.to_str().ok())
            .find(|id| {
                !id.is_empty()
                    && id.len() <= 128
                    && id
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            })
            .map(str::to_owned);

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
                &model,
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

        let resp_headers = res.headers().clone();
        if status >= 400 {
            observed.diagnostics().stage = "http_body".into();
            let mut bytes = Vec::new();
            loop {
                match res.chunk().await {
                    Ok(Some(chunk)) => {
                        observed.diagnostics().received_bytes += chunk.len() as u64;
                        bytes.extend_from_slice(&chunk);
                    }
                    Ok(None) => break,
                    Err(error) => {
                        observed.diagnostics().error_detail = transport_detail(error);
                        observed
                            .record(Some("upstream_error_body_error"), failoverable(status, ""));
                        return Err((
                            status,
                            format!("{}: {status} upstream error body interrupted", acc.email),
                        ));
                    }
                }
            }
            let snippet = String::from_utf8_lossy(&bytes);
            let code = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| {
                    v.get("error")
                        .unwrap_or(&v)
                        .get("code")
                        .and_then(|code| code.as_str())
                        .and_then(error_code)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| format!("http_{status}"));
            observed.record(Some(&code), false);
            if status == 400 {
                if let Some(param) = rejected_param(&snippet) {
                    if let Some(nb) = remove_param(&body, &param) {
                        if !codex_client {
                            app.learn_strip(param);
                        }
                        body = nb;
                        observed = request_log.begin(app, acc);
                        continue;
                    }
                }
            }
            if failoverable(status, &snippet) {
                app.pool.mark_failure(&acc.id, &model);
                return Err((status, format!("{}: {status} {snippet}", acc.email)));
            }
            // genuine client error: pass through untouched
            let mut out = Response::builder()
                .status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST));
            copy_headers(out.headers_mut().unwrap(), &resp_headers);
            return out
                .body(Body::from(bytes))
                .map_err(|e| (500, e.to_string()));
        }
        break res;
    };
    let status = res.status().as_u16();
    let resp_headers = res.headers().clone();

    // success status, BUT the upstream may carry in-stream error events
    // inside 200 streams (chatgpt.com sends server_is_overloaded/slow_down
    // after only lifecycle events). Buffer upstream chunks until the first
    // CONTENT event: an error arriving before any content is still
    // failoverable — nothing user-visible has reached the client. Progress
    // markers are the content-bearing events (output items, deltas); the
    // bare lifecycle frames (created/in_progress) carry nothing.
    let mut buffered = Vec::new();
    let mut remainder = Bytes::new();
    observed.stats = StreamStats::new(
        resp_headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
    );
    observed.diagnostics().stage = "prefix".into();
    loop {
        let ended = match res.chunk().await {
            Ok(Some(c)) => {
                observed.diagnostics().received_bytes += c.len() as u64;
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
            Err(error) => {
                observed.transport_error(error);
                return Err((0, format!("{}: upstream stream error", acc.email)));
            }
        };
        // The same observer spans every chunk, including an incomplete
        // event carried over into the streaming phase. Event order decides
        // whether a coalesced content/error pair can still fail over.
        if let Some(code) = observed.stats.error_before_content.clone() {
            observed.record(None, false);
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
    app.pool.pin(session_key, &acc.id, &model);
    if observed.entry.is_some() {
        observed.diagnostics().stage = "stream".into();
    }
    let mut out =
        Response::builder().status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK));
    copy_headers(out.headers_mut().unwrap(), &resp_headers);
    // Rewrite every default-window field together, not just its percentage:
    // a serving Free account's monthly duration must not relabel weekly usage.
    if resp_headers.contains_key("x-codex-primary-used-percent")
        || resp_headers.contains_key("x-codex-secondary-used-percent")
    {
        rewrite_pool_usage_headers(
            out.headers_mut().unwrap(),
            &pool_usage_windows(app).unwrap_or_default(),
            crate::store::now_secs(),
        );
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
                    observed.diagnostics().received_bytes += c.len() as u64;
                    observed.stats.push(&c);
                    yield Ok::<_, std::io::Error>(c);
                }
                Err(e) => {
                    // A body consumer may stop polling as soon as it receives
                    // Err, so finalize the upstream failure before yielding it.
                    observed.transport_error(e);
                    yield Err(std::io::Error::other("upstream stream error"));
                    return;
                }
            }
        }
        observed.eof();
    };
    out.body(Body::from_stream(stream))
        .map_err(|e| (500, e.to_string()))
}

struct RequestLog {
    entry: LogEntry,
    started: Instant,
    attempts: u32,
}

impl RequestLog {
    fn begin(&mut self, app: &App, acc: &Account) -> StreamLog {
        self.attempts += 1;
        let mut entry = self.entry.clone();
        entry.account_id = acc.id.clone();
        entry.account_email = acc.email.clone();
        entry.diagnostics.as_mut().unwrap().attempt = self.attempts;
        StreamLog {
            stats: StreamStats::default(),
            store: app.store.clone(),
            pool: app.pool.clone(),
            account_id: acc.id.clone(),
            entry: Some(entry),
            request_started: self.started,
            attempt_started: Instant::now(),
        }
    }
}

// Only log a bounded transport cause chain; reqwest's URL can contain secrets.
// HTTP/SSE error bodies and messages are deliberately not copied to diagnostics.
fn transport_detail(error: reqwest::Error) -> String {
    let error = error.without_url();
    let mut detail = format!(
        "timeout={} connect={} body={} decode={}",
        error.is_timeout(),
        error.is_connect(),
        error.is_body(),
        error.is_decode()
    );
    let mut cause: Option<&dyn std::error::Error> = Some(&error);
    for _ in 0..8 {
        let Some(current) = cause else { break };
        detail.push_str(": ");
        detail.push_str(
            &current
                .to_string()
                .split_whitespace()
                .map(|word| if word.contains("://") { "<url>" } else { word })
                .collect::<Vec<_>>()
                .join(" "),
        );
        cause = current.source();
    }
    detail.chars().take(2048).collect()
}

fn error_code(code: &str) -> Option<&str> {
    (!code.is_empty()
        && code.len() <= 80
        && code.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'))
    .then_some(code)
}

/// Finalize once even when a response is dropped before its first poll or
/// while suspended at a yield. User cancellation alone never cools an account.
struct StreamLog {
    stats: StreamStats,
    store: Store,
    pool: pool::Pool,
    account_id: String,
    entry: Option<LogEntry>,
    request_started: Instant,
    attempt_started: Instant,
}

impl StreamLog {
    fn diagnostics(&mut self) -> &mut RequestDiagnostics {
        self.entry.as_mut().unwrap().diagnostics.as_mut().unwrap()
    }

    fn eof(&mut self) {
        self.stats.finish();
        self.record(None, false);
    }

    fn transport_error(&mut self, error: reqwest::Error) {
        self.diagnostics().error_detail = transport_detail(error);
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
        let diagnostics = entry.diagnostics.as_mut().unwrap();
        diagnostics.elapsed_ms = self.request_started.elapsed().as_millis() as u64;
        diagnostics.attempt_ms = self.attempt_started.elapsed().as_millis() as u64;
        diagnostics.content_seen = self.stats.content_seen;
        diagnostics.completed = self.stats.completed;
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
        }
        log::log!(
            if entry.error.is_empty() {
                log::Level::Info
            } else {
                log::Level::Warn
            },
            "request attempt: model={:?} account={:?} status={} error={:?} diagnostics={}",
            entry.model,
            entry.account_email,
            entry.status,
            entry.error,
            serde_json::to_string(&entry.diagnostics).expect("request diagnostics")
        );
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
                .and_then(error_code)
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
/// weights (1.0 when nothing is calibrated yet), independently within each
/// duration group. Shared by the usage poll and outbound header rewrite.
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

/// Outbound headers use the latest account-level probe snapshots and the same
/// duration grouping as the usage endpoint, ignoring disabled/deleted accounts.
fn pool_usage_windows(app: &App) -> Option<Vec<PoolUsageWindow>> {
    let accounts = app.store.list_accounts().ok()?;
    let snap = app.pool.snapshot();
    let mut entries = Vec::new();
    for acc in accounts.into_iter().filter(|a| !a.disabled) {
        if let Some(q) = snap.get(&acc.id).and_then(|m| m.get("default")) {
            let w = app
                .store
                .calibration(&acc.id)
                .ok()
                .flatten()
                .map(|c| c.tokens_per_pct);
            entries.push((
                Limit {
                    primary: Window {
                        used_pct: Some(q.primary_pct),
                        window_seconds: Some(q.primary_window_secs),
                        reset_at: Some(q.primary_reset_at),
                    },
                    secondary: Window {
                        used_pct: Some(q.secondary_pct),
                        window_seconds: Some(q.secondary_window_secs),
                        reset_at: Some(q.secondary_reset_at),
                    },
                    ..Default::default()
                },
                w,
            ));
        }
    }
    Some(aggregate_usage_windows(&entries))
}

fn rewrite_pool_usage_headers(headers: &mut HeaderMap, windows: &[PoolUsageWindow], now: i64) {
    for slot in ["primary", "secondary"] {
        for suffix in [
            "used-percent",
            "window-minutes",
            "reset-at",
            "reset-after-seconds",
        ] {
            headers.remove(format!("x-codex-{slot}-{suffix}"));
        }
    }
    headers.insert(
        "x-codex-plan-type",
        axum::http::HeaderValue::from_static("pro"),
    );
    // No known pool window: omit the single account's quota instead of
    // presenting it as a pool aggregate or manufacturing zero usage.
    for (i, window) in windows.iter().enumerate() {
        let prefix = match i {
            0 => "x-codex-primary".to_string(),
            1 => "x-codex-secondary".to_string(),
            _ => format!("x-herdex-pool-{}-primary", window.seconds),
        };
        let mut fields = vec![
            ("used-percent", window.pct.round() as i64),
            ("window-minutes", window.seconds / 60),
        ];
        if let Some(at) = window.reset_at {
            fields.push(("reset-at", at));
            fields.push(("reset-after-seconds", (at - now).max(0)));
        }
        for (suffix, value) in fields {
            headers.insert(
                axum::http::HeaderName::from_bytes(format!("{prefix}-{suffix}").as_bytes())
                    .unwrap(),
                axum::http::HeaderValue::from_str(&value.to_string()).unwrap(),
            );
        }
        if i >= 2 {
            headers.insert(
                axum::http::HeaderName::from_bytes(
                    format!("x-herdex-pool-{}-limit-name", window.seconds).as_bytes(),
                )
                .unwrap(),
                axum::http::HeaderValue::from_static("herdex pool"),
            );
        }
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
    async fn transport_diagnostics_remove_url_credentials_and_query_parameters() {
        let error = reqwest::Client::new()
            .get("ftp://username:transport-secret@example.test/responses?token=query-secret")
            .send()
            .await
            .unwrap_err();
        let detail = transport_detail(error);
        for secret in [
            "username",
            "transport-secret",
            "query-secret",
            "example.test",
            "ftp://",
        ] {
            assert!(
                !detail.contains(secret),
                "URL data must not appear in diagnostics"
            );
        }
        assert!(
            detail.contains("scheme"),
            "keep the underlying cause: {detail}"
        );
    }

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
                        service_tier: Some("priority".into()),
                        status: 200,
                        diagnostics: Some(RequestDiagnostics::default()),
                        ..Default::default()
                    }),
                    request_started: Instant::now(),
                    attempt_started: Instant::now(),
                };
                observed.stats.push(b"data:{\"type\":\"token_count\",\"usage\":{\"input_tokens\":120,\"cached_tokens\":80,\"output_tokens\":45}}\n\n");
                match outcome {
                    "completed" => observed
                        .stats
                        .push(b"data:{\"type\":\"response.completed\"}\n\n"),
                    "eof" => observed.eof(),
                    "transport_error" => observed.transport_error(
                        reqwest::Client::new()
                            .get("http://[invalid")
                            .send()
                            .await
                            .unwrap_err(),
                    ),
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
                assert_eq!(logs[0].service_tier.as_deref(), Some("priority"));
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
    fn pool_windows_count_an_account_once_per_duration() {
        let window = |pct, reset| Window {
            used_pct: Some(pct),
            window_seconds: Some(604800),
            reset_at: Some(reset),
        };
        let windows = aggregate_usage_windows(&[
            (
                Limit {
                    primary: window(20.0, 100),
                    secondary: window(40.0, 200),
                    ..Default::default()
                },
                Some(2.0),
            ),
            (
                Limit {
                    primary: window(80.0, 300),
                    ..Default::default()
                },
                Some(1.0),
            ),
        ]);
        assert_eq!(windows.len(), 1);
        assert!((windows[0].pct - 160.0 / 3.0).abs() < 1e-9);
        assert_eq!(windows[0].reset_at, Some(200));
    }

    #[test]
    fn pool_windows_require_a_known_finite_percentage_and_valid_duration() {
        for (pct, seconds) in [
            (None, Some(604800)),
            (Some(f64::NAN), Some(604800)),
            (Some(f64::INFINITY), Some(604800)),
            (Some(20.0), None),
            (Some(20.0), Some(0)),
            (Some(20.0), Some(-1)),
            (Some(20.0), Some(i64::MAX)),
        ] {
            assert!(PoolUsageWindow::from_window(&Window {
                used_pct: pct,
                window_seconds: seconds,
                reset_at: Some(100),
            })
            .is_none());
        }
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
