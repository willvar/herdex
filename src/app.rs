//! Shared application state and cross-module services.

use crate::config::Config;
use crate::oauth;
use crate::pool::Pool;
use crate::store::{Account, Store};
use crate::usage;
use axum::Router;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct App {
    pub cfg: Config,
    pub store: Store,
    pub pool: Pool,
    pub http: reqwest::Client,
    pub usage_root: String,
    pub pending: Mutex<HashMap<String, oauth::PKCE>>,
    /// per-account refresh single-flight: concurrent refreshes would replay
    /// the same refresh token — OpenAI's replay detection then kills the
    /// whole token family ("already been used" 401s, unrecoverable)
    pub refresh_guards: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// unix day of the last request_log prune (0 = never)
    pub last_prune_day: std::sync::atomic::AtomicI64,
    /// upstream-rejected body params learned from 400s (non-codex clients
    /// get these pre-stripped so steady state costs no extra upstream request)
    pub learned_strips: Mutex<std::collections::HashSet<String>>,
}

impl App {
    /// Path of the learned-strips cache file (survives restarts).
    pub fn learned_path(&self) -> String {
        format!(
            "{}/learned-strips.json",
            self.cfg.state_root.trim_end_matches('/')
        )
    }

    /// Loads the learned-strips set from disk; missing/corrupt file -> empty.
    pub fn load_learned_strips(cfg: &Config) -> std::collections::HashSet<String> {
        let path = format!(
            "{}/learned-strips.json",
            cfg.state_root.trim_end_matches('/')
        );
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Records a newly rejected param and persists the set (best-effort).
    pub fn learn_strip(&self, param: String) {
        let changed = self.learned_strips.lock().unwrap().insert(param);
        if changed {
            let json = serde_json::to_string(&*self.learned_strips.lock().unwrap())
                .unwrap_or_else(|_| "[]".into());
            let _ = std::fs::write(self.learned_path(), json);
        }
    }
}

pub type AppHandle = Arc<App>;

impl App {
    /// Client identity strategy: a genuine codex client's own headers are the
    /// most authentic thing we can send (self-updating version, correct
    /// platform string) — pass them through untouched. Third-party clients
    /// get masked with the configured codex persona; their own UA/originator
    /// can trigger Cloudflare challenges upstream.
    pub fn is_codex_client(h: &reqwest::header::HeaderMap) -> bool {
        let originator = h
            .get("Originator")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if originator.to_lowercase().contains("codex") {
            return true;
        }
        let ua = h
            .get("User-Agent")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        ua.starts_with("codex_cli_rs/") || ua.starts_with("Codex ")
    }

    /// Applies the header impersonation rules to an outbound request builder:
    /// defaults first (the codex persona), then the genuine client's identity
    /// if it is one, then session identifiers.
    pub fn apply_identity(
        &self,
        rb: reqwest::RequestBuilder,
        client_headers: &reqwest::header::HeaderMap,
    ) -> reqwest::RequestBuilder {
        let mut rb = rb;
        for (k, v) in &self.cfg.header_defaults {
            rb = rb.header(k, v);
        }
        if Self::is_codex_client(client_headers) {
            for k in ["User-Agent", "Originator", "Version"] {
                if let Some(v) = client_headers.get(k) {
                    rb = rb.header(k, v);
                }
            }
        }
        for k in ["Session-Id", "X-Client-Request-Id"] {
            if let Some(v) = client_headers.get(k) {
                rb = rb.header(k, v);
            }
        }
        rb
    }

    /// Force-refreshes the account's tokens via the refresh grant.
    /// Refreshes one account's tokens — SINGLE-FLIGHT per account. The
    /// refresh token rotates on every use: two concurrent refreshes with the
    /// same token means one of them is a replay, and OpenAI's replay
    /// detection kills the whole token family. Always re-read the stored
    /// tokens right before the call so the freshest rotation wins.
    pub async fn refresh_expired(&self, acc: &mut Account) -> Result<(), String> {
        let guard = {
            let mut g = self.refresh_guards.lock().await;
            g.entry(acc.id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = guard.lock().await;
        // freshest tokens from the DB — a concurrent flight may have just
        // rotated them under us
        if let Ok(latest) = self.store.list_accounts() {
            if let Some(latest) = latest.iter().find(|x| x.id == acc.id) {
                if latest.expires_at > acc.expires_at || latest.access_token != acc.access_token {
                    acc.access_token = latest.access_token.clone();
                    acc.refresh_token = latest.refresh_token.clone();
                    acc.expires_at = latest.expires_at;
                }
            }
        }
        let ts = oauth::refresh(
            &self.http,
            &self.cfg.oauth.issuer,
            &self.cfg.oauth.client_id,
            &acc.refresh_token,
        )
        .await?;
        self.store.record_account_error(&acc.id, "");
        let mut exp = acc.expires_at;
        if let Ok(claims) = oauth::parse_id_token(&ts.id_token) {
            exp = claims.exp;
        }
        let exp = if ts.expires_in > 0 {
            crate::store::now_secs() + ts.expires_in
        } else {
            exp
        };
        self.store
            .update_tokens(&acc.id, &ts.access_token, &ts.refresh_token, exp)?;
        acc.access_token = ts.access_token;
        acc.refresh_token = ts.refresh_token;
        acc.expires_at = exp;
        log::info!("token refreshed for {}", acc.email);
        Ok(())
    }

    /// Probes the zero-cost usage endpoint for one account and records the
    /// windows into the pool ("default" for the main limit, per-model for
    /// additional limits like spark). Backfills missing plan_type.
    pub async fn fetch_usage(&self, acc: &Account) -> Result<usage::Report, String> {
        let report = usage::fetch(
            &self.http,
            &self.usage_root,
            &acc.access_token,
            &acc.account_id,
            &self.cfg.header_defaults,
        )
        .await?;
        if acc.plan_type.is_empty() && !report.plan_type.is_empty() {
            let _ = self.store.set_account_plan(&acc.id, &report.plan_type);
        }
        self.observe_usage(&acc.id, &report);
        Ok(report)
    }

    /// Records a usage report's windows into the pool ("default" for the main
    /// limit, per-model for additional limits like spark).
    ///
    /// This is the single probe-persistence point for every report path —
    /// background pollers, CLI redeem/refresh and the panel quota check all
    /// land here. add_probe dedupes only when the observed quota, metadata
    /// and completed-request watermark are all unchanged.
    pub fn observe_usage(&self, acc_id: &str, report: &usage::Report) {
        let now = crate::store::now_secs();
        if report.main.primary.used_pct.is_some() || report.main.primary.reset_at.is_some() {
            self.pool.observe(
                acc_id,
                "default",
                crate::pool::Quota {
                    primary_pct: report.main.primary.used_pct.unwrap_or(0.0),
                    secondary_pct: report.main.secondary.used_pct.unwrap_or(0.0),
                    primary_reset_at: report.main.primary.reset_at.unwrap_or(0),
                    secondary_reset_at: report.main.secondary.reset_at.unwrap_or(0),
                    primary_window_secs: report.main.primary.window_seconds.unwrap_or(0),
                    secondary_window_secs: report.main.secondary.window_seconds.unwrap_or(0),
                    observed_at: now,
                },
            );
        }
        for a in &report.additional {
            self.pool.observe(
                acc_id,
                &usage::model_key(&a.name),
                crate::pool::Quota {
                    primary_pct: a.limit.primary.used_pct.unwrap_or(0.0),
                    secondary_pct: a.limit.secondary.used_pct.unwrap_or(0.0),
                    primary_reset_at: a.limit.primary.reset_at.unwrap_or(0),
                    secondary_reset_at: a.limit.secondary.reset_at.unwrap_or(0),
                    primary_window_secs: a.limit.primary.window_seconds.unwrap_or(0),
                    secondary_window_secs: a.limit.secondary.window_seconds.unwrap_or(0),
                    observed_at: now,
                },
            );
        }
        if let (Some(p), Some(r)) = (report.main.primary.used_pct, report.main.primary.reset_at) {
            self.store
                .add_probe(acc_id, crate::store::now_secs(), p, r, &report.plan_type);
        }
    }

    /// Consumes one banked reset credit, then re-probes usage.
    pub async fn consume_reset(&self, acc: &Account) -> Result<usage::Report, String> {
        let redeem = format!("{}-{}", crate::store::now_secs(), uuid::Uuid::new_v4());
        usage::consume_reset_credit(
            &self.http,
            &self.usage_root,
            &acc.access_token,
            &acc.account_id,
            &self.cfg.header_defaults,
            &redeem,
        )
        .await?;
        let report = self.fetch_usage(acc).await?;
        Ok(report)
    }

    pub fn state_json(&self) -> serde_json::Value {
        let accounts: Vec<serde_json::Value> = self
            .store
            .list_accounts()
            .unwrap_or_default()
            .iter()
            .map(|a| {
                json!({
                    "id": a.id, "email": a.email, "plan_type": a.plan_type,
                    "disabled": a.disabled,
                    "expires_at": rfc3339(a.expires_at),
                    "last_error": a.last_error,
                    "last_error_at": rfc3339_or_empty(a.last_error_at),
                })
            })
            .collect();
        let keys: Vec<serde_json::Value> = self
            .store
            .list_api_keys()
            .unwrap_or_default()
            .iter()
            .map(|k| json!({"key": k.key, "comment": k.comment, "created_at": rfc3339(k.created_at), "disabled": k.disabled}))
            .collect();
        let quota = self.pool.snapshot();
        let email_of: HashMap<String, String> = self
            .store
            .list_accounts()
            .unwrap_or_default()
            .into_iter()
            .map(|a| (a.id, a.email))
            .collect();
        let mut quota_by_email = serde_json::Map::new();
        for (acc_id, models) in &quota {
            if let Some(email) = email_of.get(acc_id) {
                let mut mm = serde_json::Map::new();
                for (model, q) in models {
                    mm.insert(model.clone(), serde_json::to_value(q).unwrap_or_default());
                }
                quota_by_email.insert(email.clone(), serde_json::Value::Object(mm));
            }
        }
        let logs: Vec<serde_json::Value> = self
            .store
            .recent_logs(100)
            .unwrap_or_default()
            .iter()
            .map(|l| serde_json::to_value(l).unwrap_or_default())
            .collect();
        json!({
            "accounts": accounts,
            "api_keys": keys,
            "settings": self.store.get_settings().unwrap_or_default(),
            "logs": logs,
            "quota": quota_by_email,
        })
    }
}

fn rfc3339(secs: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(secs)
        .map(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

fn rfc3339_or_empty(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    rfc3339(secs)
}

/// Builds the upstream HTTP client the way the official codex CLI does:
/// rustls with native roots, Cloudflare-capable cookie store.
pub fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(true)
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("http client")
}

/// Assembles the complete router (client + manage + panel redirect +
/// chatgpt-backend fallback proxy).
/// Extracted so tests can catch route conflicts without starting a server.
pub fn build_router(app: AppHandle) -> axum::Router {
    // axum defaults to 2MB request bodies, which a single base64 image in a
    // codex request would blow through; local/LAN only, so be generous
    let client = crate::proxy::router().layer(axum::extract::DefaultBodyLimit::max(64 << 20));
    let manage_router = crate::manage::router();
    Router::new()
        .merge(client)
        .merge(manage_router)
        .route(
            "/manage",
            axum::routing::get(|| async { axum::response::Redirect::temporary("/manage/panel/") }),
        )
        .route(
            "/manage/panel",
            axum::routing::get(|| async { axum::response::Redirect::temporary("/manage/panel/") }),
        )
        .route(
            "/manage/panel/",
            axum::routing::get(crate::panel::serve_root),
        )
        .route(
            "/manage/panel/{*rest}",
            axum::routing::get(crate::panel::serve),
        )
        .fallback(crate::proxy::codex_backend_fallback)
        .with_state(app)
}

/// Background proactive token refresh: every 2 minutes, refresh accounts
/// whose tokens expire within 10 minutes.
pub fn spawn_refresh_loop(app: AppHandle) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(120)).await;
            // daily request_log prune (retention-days, 0 = keep forever)
            let days = app.cfg.retention_days();
            if days > 0 {
                let today = crate::store::now_secs() / 86400;
                let last = app
                    .last_prune_day
                    .load(std::sync::atomic::Ordering::Relaxed);
                if last != today {
                    match app.store.prune_logs(days) {
                        Ok(n) if n > 0 => {
                            log::info!("pruned {n} request_log rows (retention {days}d)")
                        }
                        Ok(_) => {}
                        Err(e) => log::warn!("prune failed: {e}"),
                    }
                    match app.store.prune_probes(31) {
                        Ok(n) if n > 0 => log::info!("pruned {n} usage_probes rows (31d)"),
                        Ok(_) => {}
                        Err(e) => log::warn!("probe prune failed: {e}"),
                    }
                    app.last_prune_day
                        .store(today, std::sync::atomic::Ordering::Relaxed);
                }
            }
            let accounts = app.store.list_accounts().unwrap_or_default();
            for mut a in accounts {
                let expires_in = a.expires_at - crate::store::now_secs();
                if a.disabled || expires_in > 600 {
                    continue;
                }
                if let Err(e) = app.refresh_expired(&mut a).await {
                    log::warn!("proactive refresh failed for {}: {e}", a.email);
                }
            }
        }
    });
}
