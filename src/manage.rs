//! Panel API under /manage/api/* (single manage key protects everything)
//! plus the embedded panel page.

use crate::app::AppHandle;
use crate::oauth;
use crate::store::{Account, ApiKey, Settings, UsageDim};
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
        .route("/manage/api/login/finish", axum::routing::post(login_finish))
        .route("/manage/api/accounts/{id}/quota", axum::routing::post(quota))
        .route(
            "/manage/api/accounts/{id}/quota/consume",
            axum::routing::post(consume),
        )
        .route("/manage/api/accounts/{id}/disable", axum::routing::post(disable))
        .route("/manage/api/accounts/{id}", axum::routing::delete(delete_account))
        .route("/manage/api/keys", axum::routing::post(add_key))
        .route(
            "/manage/api/keys/{key}",
            axum::routing::patch(patch_key).delete(delete_key),
        )
        .route("/manage/api/settings", axum::routing::put(put_settings))
        .route("/manage/api/usage", axum::routing::get(usage))
        .route("/manage/api/calibration", axum::routing::get(calibration))
}

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
    let days: i64 = q.get("days").and_then(|v| v.parse().ok()).unwrap_or(7).clamp(1, 90);
    let store = &st.store;
    let r = serde_json::json!({
        "daily": store.usage_daily(days).unwrap_or_default(),
        "by_account": store.usage_by(UsageDim::Account, days).unwrap_or_default(),
        "by_key": store.usage_by(UsageDim::Key, days).unwrap_or_default(),
        "by_model": store.usage_by(UsageDim::Model, days).unwrap_or_default(),
    });
    ([(axum::http::header::CONTENT_TYPE, "application/json")], r.to_string()).into_response()
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
    for a in app.store.list_accounts().unwrap_or_default() {
        let pct = snapshot
            .get(&a.id)
            .and_then(|m| m.get("default"))
            .map(|q| q.primary_pct.max(q.secondary_pct));
        let cal = match app.store.calibration(&a.id) {
            Ok(Some(v)) => v,
            _ => crate::store::Calibration::default(),
        };
        let tpp = cal.tokens_per_pct;
        let samples = cal.samples;
        let is_calibrated = tpp > 0.0;
        let remaining_tokens = if is_calibrated {
            pct.map(|p| tpp * (100.0 - p)).unwrap_or(0.0)
        } else {
            0.0
        };
        if is_calibrated {
            calibrated += 1;
            if pct.is_some() {
                total_remaining += remaining_tokens;
            }
        }
        rows.push(serde_json::json!({
            "email": a.email, "plan_type": a.plan_type,
            "tokens_per_pct": if is_calibrated { serde_json::json!(tpp) } else { serde_json::Value::Null },
            "samples": samples,

            "used_pct": pct,
            "remaining_tokens": if is_calibrated { serde_json::json!(remaining_tokens) } else { serde_json::Value::Null },
            "calibrated": is_calibrated,
        }));
    }
    let r = serde_json::json!({
        "accounts": rows,
        "calibrated_accounts": calibrated,
        "pool_remaining_tokens": if calibrated > 0 { serde_json::json!(total_remaining) } else { serde_json::Value::Null },
    });
    ([(axum::http::header::CONTENT_TYPE, "application/json")], r.to_string()).into_response()
}

async fn state(State(app): State<AppHandle>, headers: HeaderMap) -> Response {
    if let Err(res) = auth_check(&app, &headers) {
        return res;
    }
    Json(app.state_json()).into_response()
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
    app.pending
        .lock()
        .unwrap()
        .insert(pkce.state.clone(), pkce);
    Json(json!({"auth_url": auth_url, "state": auth_url.split("state=").nth(1).unwrap_or("").split('&').next().unwrap_or("")})).into_response()
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
        return err_json(StatusCode::BAD_REQUEST, "callback url has no code parameter");
    }
    let pkce = {
        let mut pending = app.pending.lock().unwrap();
        pending.remove(&state_param)
    };
    let Some(pkce) = pkce else {
        return err_json(StatusCode::BAD_REQUEST, "unknown state (already finished or expired?)");
    };
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
        Err(e) => return err_json(StatusCode::BAD_GATEWAY, format!("token exchange: {e}")),
    };
    let claims = match oauth::parse_id_token(&ts.id_token) {
        Ok(c) => c,
        Err(e) => return err_json(StatusCode::BAD_GATEWAY, e),
    };
    if claims.account_id().is_empty() {
        return err_json(StatusCode::BAD_GATEWAY, "id_token has no chatgpt_account_id");
    }
    let expires_at = if ts.expires_in > 0 {
        crate::store::now_secs() + ts.expires_in
    } else {
        claims.exp
    };
    let acc = Account {
        id: claims.account_id(),
        email: claims.email.clone(),
        plan_type: claims.plan_type(),
        account_id: claims.account_id(),
        access_token: ts.access_token,
        refresh_token: ts.refresh_token,
        expires_at,
        ..Default::default()
    };
    let (id, email, plan) = (acc.id.clone(), acc.email.clone(), acc.plan_type.clone());
    if let Err(e) = app.store.upsert_account(&acc) {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    Json(json!({"id": id, "email": email, "plan_type": plan})).into_response()
}

#[derive(Deserialize)]
struct LoginFinishReq {
    callback_url: String,
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
        Ok(report) => Json(report).into_response(),
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
        Ok(report) => Json(report).into_response(),
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

async fn add_key(State(app): State<AppHandle>, headers: HeaderMap, Json(req): Json<AddKeyReq>) -> Response {
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
    let mut key = key;
    if let Some(new_key) = &req.key {
        if new_key.is_empty() {
            return err_json(StatusCode::BAD_REQUEST, "key cannot be empty");
        }
        if let Err(e) = app.store.update_api_key(&key, new_key) {
            return err_json(StatusCode::CONFLICT, e);
        }
        key = new_key.clone();
    }
    if let Some(comment) = &req.comment {
        if let Err(e) = app.store.set_api_key_comment(&key, comment) {
            return err_json(StatusCode::NOT_FOUND, e);
        }
    }
    if let Some(disabled) = &req.disabled {
        if let Err(e) = app.store.set_api_key_disabled(&key, *disabled) {
            return err_json(StatusCode::NOT_FOUND, e);
        }
    }
    Json(json!({"ok": true})).into_response()
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
    match app.store.put_settings(&settings) {
        Ok(()) => Json(settings).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

// keep imports used
#[allow(dead_code)]
fn _unused(_: &HashMap<String, String>) {}
type _K = ApiKey;
type _S = Settings;
