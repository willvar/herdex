//! Panel API under /manage/api/* (single manage key protects everything)
//! plus the embedded panel page.

use crate::app::AppHandle;
use crate::oauth;
use crate::store::{Account, LogQuery, Settings, UsageDim};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;

pub fn router() -> axum::Router<AppHandle> {
    axum::Router::new()
        .route("/manage/api/state", axum::routing::get(state))
        .route("/manage/api/login/start", axum::routing::post(login_start))
        .route(
            "/manage/api/login/finish",
            axum::routing::post(login_finish),
        )
        .route(
            "/manage/api/accounts/{id}/quota",
            axum::routing::post(quota),
        )
        .route(
            "/manage/api/accounts/{id}/quota/consume",
            axum::routing::post(consume),
        )
        .route(
            "/manage/api/accounts/{id}/disable",
            axum::routing::post(disable),
        )
        .route(
            "/manage/api/accounts/{id}",
            axum::routing::delete(delete_account),
        )
        .route("/manage/api/keys", axum::routing::post(add_key))
        .route(
            "/manage/api/keys/{key}",
            axum::routing::patch(patch_key).delete(delete_key),
        )
        .route("/manage/api/settings", axum::routing::put(put_settings))
        .route("/manage/api/logs", axum::routing::get(logs))
        .route("/manage/api/usage", axum::routing::get(usage))
        .route("/manage/api/calibration", axum::routing::get(calibration))
        .route("/manage/api/history", axum::routing::get(history))
        .layer(axum::middleware::map_response(
            |mut response: Response| async move {
                response.headers_mut().insert(
                    axum::http::header::CACHE_CONTROL,
                    axum::http::HeaderValue::from_static("no-store"),
                );
                response
            },
        ))
}

// Err carries an axum Response; boxing it everywhere for a lint is not worth it
#[allow(clippy::result_large_err)]
fn auth_check(app: &AppHandle, headers: &HeaderMap) -> Result<(), Response> {
    let key = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if key.is_empty() || key != app.cfg.manage.key {
        return Err((StatusCode::UNAUTHORIZED, "invalid manage key").into_response());
    }
    Ok(())
}

fn err_json(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({"error": msg.into()}))).into_response()
}

async fn usage(
    State(st): State<AppHandle>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Err(res) = auth_check(&st, &headers) {
        return res;
    }
    let days: i64 = q
        .get("days")
        .and_then(|v| v.parse().ok())
        .unwrap_or(7)
        .clamp(1, 90);
    let store = &st.store;
    let result = (|| -> Result<serde_json::Value, String> {
        Ok(
            json!({ "daily": store.usage_daily(days)?, "by_account": store.usage_by(UsageDim::Account,days)?,
            "by_key": store.usage_by(UsageDim::Key,days)?, "by_model": store.usage_by(UsageDim::Model,days)?, "timezone":"Asia/Hong_Kong" }),
        )
    })();
    match result {
        Ok(value) => Json(value).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// Probe history + per-day per-model burns for the panel's charts.
async fn history(
    State(app): State<AppHandle>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Err(res) = auth_check(&app, &headers) {
        return res;
    }
    let days: i64 = q
        .get("days")
        .and_then(|v| v.parse().ok())
        .unwrap_or(7)
        .clamp(1, 90);
    let store = &app.store;
    match (store.probe_history(days), store.usage_daily_by_model(days)) {
        (Ok(probes), Ok(model_daily)) => {
            Json(json!({"probes":probes,"model_daily":model_daily})).into_response()
        }
        (Err(e), _) | (_, Err(e)) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn logs(
    State(app): State<AppHandle>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<LogQuery>,
) -> Response {
    if let Err(response) = auth_check(&app, &headers) {
        return response;
    }
    if query
        .since
        .zip(query.until)
        .is_some_and(|(from, to)| from > to)
    {
        return err_json(StatusCode::BAD_REQUEST, "invalid time range");
    }
    match app.store.query_logs(&query) {
        Ok(page) => Json(page).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// Per-account capacity calibration (empirical tokens-per-1%) plus remaining
/// capacity estimates, for the panel's 容量估算 card.
async fn calibration(State(app): State<AppHandle>, headers: HeaderMap) -> Response {
    if let Err(res) = auth_check(&app, &headers) {
        return res;
    }
    let snapshot = app.pool.snapshot();
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut total_remaining: f64 = 0.0;
    let mut calibrated = 0usize;
    let mut active_calibrated = 0usize;
    let mut active_accounts = 0usize;
    let mut active_known = 0usize;
    let accounts = match app.store.list_accounts() {
        Ok(a) => a,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    for a in accounts {
        if !a.disabled {
            active_accounts += 1;
        }
        let pct = snapshot
            .get(&a.id)
            .and_then(|m| m.get("default"))
            .and_then(|q| {
                [
                    (q.primary_known, q.primary_pct),
                    (q.secondary_known, q.secondary_pct),
                ]
                .into_iter()
                .filter(|(known, pct)| *known != Some(false) && pct.is_finite())
                .map(|(_, pct)| pct)
                .reduce(f64::max)
            });
        let cal = match app.store.calibration(&a.id) {
            Ok(Some(v)) => v,
            _ => crate::store::Calibration::default(),
        };
        let tpp = cal.tokens_per_pct;
        let samples = cal.samples;
        let is_calibrated = tpp > 0.0;
        let remaining_tokens = if is_calibrated {
            pct.map(|p| tpp * (100.0 - p).max(0.0))
        } else {
            None
        };
        if is_calibrated {
            calibrated += 1;
            if !a.disabled {
                active_calibrated += 1;
                if let Some(remaining) = remaining_tokens {
                    total_remaining += remaining;
                    active_known += 1;
                }
            }
        }
        rows.push(serde_json::json!({
            "account_id": a.id, "email": a.email, "plan_type": a.plan_type, "disabled":a.disabled,
            "tokens_per_pct": if is_calibrated { serde_json::json!(tpp) } else { serde_json::Value::Null },
            "samples": samples,
            // era-mix-aware: the per-model rates let the panel (and the
            // scheduler later) price the CURRENT mix instead of a blend
            "per_model": cal.per_model,

            "used_pct": pct,
            "remaining_tokens": remaining_tokens,
            "calibrated": is_calibrated,
        }));
    }
    let r = serde_json::json!({
        "accounts": rows,
        "calibrated_accounts": calibrated,
        "active_accounts":active_accounts,"active_calibrated_accounts":active_calibrated,"active_known_accounts":active_known,
        "pool_remaining_tokens":if active_accounts==0 || active_known>0 {json!(total_remaining)}else{serde_json::Value::Null},
    });
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        r.to_string(),
    )
        .into_response()
}

async fn state(State(app): State<AppHandle>, headers: HeaderMap) -> Response {
    if let Err(res) = auth_check(&app, &headers) {
        return res;
    }
    match app.state_json() {
        Ok(value) => Json(value).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

fn callback_uri() -> String {
    "http://localhost:1455/auth/callback".into()
}

async fn login_start(State(app): State<AppHandle>, headers: HeaderMap) -> Response {
    if let Err(res) = auth_check(&app, &headers) {
        return res;
    }
    let pkce = match oauth::new_pkce() {
        Ok(p) => p,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let auth_url = oauth::auth_url(
        &app.cfg.oauth.issuer,
        &app.cfg.oauth.client_id,
        &callback_uri(),
        &pkce,
    );
    {
        let mut pending = app.pending.lock().unwrap();
        pending.retain(|_, p| crate::store::now_secs() - p.created_at < 600);
        if pending.len() >= 128 {
            return err_json(
                StatusCode::TOO_MANY_REQUESTS,
                "too many pending logins; retry later",
            );
        }
        pending.insert(pkce.state.clone(), pkce);
    }
    Json(json!({"auth_url": auth_url, "expires_at":crate::store::now_secs()+600,"state": auth_url.split("state=").nth(1).unwrap_or("").split('&').next().unwrap_or("")})).into_response()
}

async fn login_finish(
    State(app): State<AppHandle>,
    headers: HeaderMap,
    Json(req): Json<LoginFinishReq>,
) -> Response {
    if let Err(res) = auth_check(&app, &headers) {
        return res;
    }
    let url = match url::Url::parse(&req.callback_url) {
        Ok(u) => u,
        Err(e) => return err_json(StatusCode::BAD_REQUEST, format!("parse callback url: {e}")),
    };
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
        || url.port_or_known_default() != Some(1455)
        || url.path() != "/auth/callback"
    {
        return err_json(
            StatusCode::BAD_REQUEST,
            "expected the complete localhost:1455/auth/callback URL",
        );
    }
    let mut code = String::new();
    let mut state_param = String::new();
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = v.to_string(),
            "state" => state_param = v.to_string(),
            _ => {}
        }
    }
    if code.is_empty() {
        return err_json(
            StatusCode::BAD_REQUEST,
            "callback url has no code parameter",
        );
    }
    let pkce = {
        let mut pending = app.pending.lock().unwrap();
        pending.remove(&state_param)
    };
    let Some(pkce) = pkce else {
        return login_error(
            StatusCode::BAD_REQUEST,
            "authorization is finished or unavailable; generate a new link",
        );
    };
    if crate::store::now_secs() - pkce.created_at >= 600 {
        return login_error(
            StatusCode::BAD_REQUEST,
            "authorization expired; generate a new link",
        );
    }
    let ts = match oauth::exchange(
        &app.http,
        &app.cfg.oauth.issuer,
        &app.cfg.oauth.client_id,
        &callback_uri(),
        &code,
        &pkce.verifier,
    )
    .await
    {
        Ok(ts) => ts,
        Err(_) => {
            return login_error(
                StatusCode::BAD_GATEWAY,
                "authorization exchange failed; generate a new link",
            )
        }
    };
    let claims = match oauth::parse_id_token(&ts.id_token) {
        Ok(c) => c,
        Err(_) => {
            return login_error(
                StatusCode::BAD_GATEWAY,
                "authorization identity could not be verified; generate a new link",
            )
        }
    };
    if claims.account_id().is_empty() {
        return login_error(
            StatusCode::BAD_GATEWAY,
            "authorization has no workspace identity; generate a new link",
        );
    }
    let Some(user_id) = claims.user_id() else {
        return login_error(
            StatusCode::BAD_GATEWAY,
            "authorization has no stable user identity; no account was changed",
        );
    };
    let issuer = app.cfg.oauth.issuer.trim_end_matches('/').to_owned();
    if let Some(expected) = &req.expected_id {
        let matches = app.store.get_account(expected).is_ok_and(|a| {
            a.issuer == issuer && a.user_id == user_id && a.account_id == claims.account_id()
        });
        if !matches {
            return login_error(
                StatusCode::CONFLICT,
                "authorization does not match the selected user/workspace; no account was changed",
            );
        }
    }
    let expires_at = if ts.expires_in > 0 {
        crate::store::now_secs() + ts.expires_in
    } else {
        claims.exp
    };
    let acc = Account {
        issuer,
        user_id,
        email: claims.email.clone(),
        plan_type: claims.plan_type(),
        account_id: claims.account_id(),
        access_token: ts.access_token,
        refresh_token: ts.refresh_token,
        expires_at,
        ..Default::default()
    };
    let (id, updated) = match app.store.save_oauth_account(&acc) {
        Ok(result) => result,
        Err(_) => {
            return login_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "account could not be saved; generate a new link",
            )
        }
    };
    let disabled = app
        .store
        .get_account(&id)
        .map(|a| a.disabled)
        .unwrap_or(false);
    Json(json!({"id":id,"email":acc.email,"plan_type":acc.plan_type,"updated":updated,"disabled":disabled})).into_response()
}

#[derive(Deserialize)]
struct LoginFinishReq {
    callback_url: String,
    #[serde(default)]
    expected_id: Option<String>,
}

fn login_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({"error":message,"reauthorize_required":true})),
    )
        .into_response()
}

async fn quota(
    State(app): State<AppHandle>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(res) = auth_check(&app, &headers) {
        return res;
    }
    let Ok(acc) = app.store.get_account(&id) else {
        return err_json(StatusCode::NOT_FOUND, "account not found");
    };
    match app.fetch_usage(&acc).await {
        // probe persistence happens inside observe_usage (single point)
        Ok(mut report) => {
            let observed = app.pool.observation(&acc.id, "default").unwrap_or_default();
            match app.fetch_reset_credits(&acc).await {
                Ok(details) => {
                    let summary = report
                        .credits
                        .get_or_insert_with(crate::usage::Credits::default);
                    summary.available = details.available_count;
                    report.reset_credits = Some(details);
                }
                Err(error) => {
                    log::warn!("reset credit details {}: {error}", acc.email);
                }
            }
            let mut value = serde_json::to_value(report).unwrap_or_default();
            value["observed_at"] = json!(observed.observed_at);
            value["observed_seq"] = json!(observed.observed_seq);
            Json(value).into_response()
        }
        Err(e) => err_json(StatusCode::BAD_GATEWAY, e),
    }
}

async fn consume(
    State(app): State<AppHandle>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(res) = auth_check(&app, &headers) {
        return res;
    }
    let Ok(acc) = app.store.get_account(&id) else {
        return err_json(StatusCode::NOT_FOUND, "account not found");
    };
    match app.consume_reset(&acc).await {
        Ok(()) => Json(json!({"consumed": true})).into_response(),
        Err(e) => err_json(StatusCode::BAD_GATEWAY, e),
    }
}

async fn disable(
    State(app): State<AppHandle>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<DisableReq>,
) -> Response {
    if let Err(res) = auth_check(&app, &headers) {
        return res;
    }
    match app.store.set_account_disabled(&id, req.disabled) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_json(StatusCode::NOT_FOUND, e),
    }
}

#[derive(Deserialize)]
struct DisableReq {
    disabled: bool,
}

async fn delete_account(
    State(app): State<AppHandle>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(res) = auth_check(&app, &headers) {
        return res;
    }
    match app.store.delete_account(&id) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_json(StatusCode::NOT_FOUND, e),
    }
}

#[derive(Deserialize)]
struct AddKeyReq {
    key: String,
    #[serde(default)]
    comment: String,
}

async fn add_key(
    State(app): State<AppHandle>,
    headers: HeaderMap,
    Json(req): Json<AddKeyReq>,
) -> Response {
    if let Err(res) = auth_check(&app, &headers) {
        return res;
    }
    if req.key.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "key required");
    }
    match app.store.add_api_key(&req.key, &req.comment) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_json(StatusCode::CONFLICT, e),
    }
}

#[derive(Deserialize, Default)]
struct PatchKeyReq {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    comment: Option<String>,
    #[serde(default)]
    disabled: Option<bool>,
}

async fn patch_key(
    State(app): State<AppHandle>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Json(req): Json<PatchKeyReq>,
) -> Response {
    if let Err(res) = auth_check(&app, &headers) {
        return res;
    }
    if req.key.is_none() && req.comment.is_none() && req.disabled.is_none() {
        return err_json(StatusCode::BAD_REQUEST, "nothing to update");
    }
    match app.store.patch_api_key(
        &key,
        req.key.as_deref(),
        req.comment.as_deref(),
        req.disabled,
    ) {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(e) => err_json(StatusCode::CONFLICT, e),
    }
}

async fn delete_key(
    State(app): State<AppHandle>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Response {
    if let Err(res) = auth_check(&app, &headers) {
        return res;
    }
    match app.store.delete_api_key(&key) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_json(StatusCode::NOT_FOUND, e),
    }
}

async fn put_settings(
    State(app): State<AppHandle>,
    headers: HeaderMap,
    Json(settings): Json<Settings>,
) -> Response {
    if let Err(res) = auth_check(&app, &headers) {
        return res;
    }
    if settings.cooldown_seconds < 0 || settings.pin_yield_gap_pp < -1 {
        return err_json(StatusCode::BAD_REQUEST, "invalid pool parameters");
    }
    match app.store.put_settings(&settings) {
        Ok(()) => Json(settings).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

// keep imports used
#[allow(dead_code)]
fn _unused(_: &HashMap<String, String>) {}
type _S = Settings;
