//! Zero-cost codex usage probes on chatgpt.com:
//!   GET  /backend-api/wham/usage                     — per-account quota windows
//!   GET  /backend-api/wham/rate-limit-reset-credits  — banked reset credits
//!   POST /backend-api/wham/rate-limit-reset-credits/consume — spend one chosen credit
//! None of these consume model quota.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Window {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_seconds: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Limit {
    #[serde(default)]
    pub allowed: bool,
    #[serde(default)]
    pub limit_reached: bool,
    #[serde(default)]
    pub primary: Window,
    #[serde(default)]
    pub secondary: Window,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct AdditionalLimit {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub limit: Limit,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Credits {
    #[serde(default)]
    pub available: i64,
    #[serde(default)]
    pub applicable: i64,
}

/// A banked reset returned by the detail endpoint. The ID stays internal to
/// the consume path; the panel only needs the aggregate count.
#[derive(Clone, Debug, Deserialize)]
pub struct ResetCredit {
    pub id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub expires_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ResetCredits {
    #[serde(default)]
    pub credits: Vec<ResetCredit>,
    #[serde(default)]
    pub available_count: i64,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Report {
    #[serde(default)]
    pub plan_type: String,
    #[serde(default)]
    pub main: Limit,
    #[serde(default)]
    pub additional: Vec<AdditionalLimit>,
    #[serde(default)]
    pub credits: Option<Credits>,
}

#[derive(Deserialize)]
struct UsagePayload {
    #[serde(default)]
    plan_type: String,
    #[serde(default)]
    rate_limit: Option<LimitJson>,
    #[serde(default)]
    additional_rate_limits: Option<Vec<AdditionalJson>>,
    #[serde(default)]
    rate_limit_reset_credits: Option<CreditsJson>,
}

#[derive(Deserialize)]
struct LimitJson {
    #[serde(default)]
    allowed: Option<bool>,
    #[serde(default)]
    limit_reached: Option<bool>,
    #[serde(default)]
    primary_window: Option<WindowJson>,
    #[serde(default)]
    secondary_window: Option<WindowJson>,
}

#[derive(Deserialize)]
struct AdditionalJson {
    #[serde(default)]
    limit_name: String,
    #[serde(default)]
    rate_limit: Option<LimitJson>,
}

#[derive(Deserialize)]
struct CreditsJson {
    #[serde(default)]
    available_count: Option<i64>,
    #[serde(default)]
    applicable_available_count: Option<i64>,
}

#[derive(Deserialize)]
struct WindowJson {
    #[serde(default)]
    used_percent: Option<f64>,
    #[serde(default)]
    reset_after_seconds: Option<i64>,
    #[serde(default)]
    reset_at: Option<i64>,
    #[serde(default)]
    limit_window_seconds: Option<i64>,
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn parse_window(w: &Option<WindowJson>) -> Window {
    let w = match w {
        Some(w) => w,
        None => return Window::default(),
    };
    Window {
        used_pct: w.used_percent,
        window_seconds: w.limit_window_seconds,
        reset_at: match (w.reset_at, w.reset_after_seconds) {
            (Some(at), _) if at > 0 => Some(at),
            (None, Some(after)) if after > 0 => Some(now_secs() + after),
            _ => None,
        },
    }
}

fn parse_limit(l: &Option<LimitJson>) -> Limit {
    match l {
        Some(l) => Limit {
            allowed: l.allowed.unwrap_or(true),
            limit_reached: l.limit_reached.unwrap_or(false),
            primary: parse_window(&l.primary_window),
            secondary: parse_window(&l.secondary_window),
        },
        None => Limit::default(),
    }
}

pub fn parse(body: &[u8]) -> Result<Report, String> {
    let raw: UsagePayload =
        serde_json::from_slice(body).map_err(|e| format!("usage payload: {e}"))?;
    let mut report = Report {
        plan_type: raw.plan_type,
        main: parse_limit(&raw.rate_limit),
        additional: Vec::new(),
        credits: None,
    };
    if let Some(adds) = raw.additional_rate_limits {
        for a in adds {
            if a.limit_name.is_empty() {
                continue;
            }
            report.additional.push(AdditionalLimit {
                name: a.limit_name,
                limit: parse_limit(&a.rate_limit),
            });
        }
    }
    if let Some(c) = raw.rate_limit_reset_credits {
        report.credits = Some(Credits {
            available: c.available_count.unwrap_or(0),
            applicable: c.applicable_available_count.unwrap_or(0),
        });
    }
    Ok(report)
}

fn headers(
    token: &str,
    account_id: &str,
    defaults: &std::collections::HashMap<String, String>,
) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    if let Ok(v) = format!("Bearer {token}").parse() {
        h.insert("Authorization", v);
    }
    if !account_id.is_empty() {
        if let Ok(v) = account_id.parse() {
            h.insert("Chatgpt-Account-Id", v);
        }
    }
    for (k, v) in defaults {
        if let (Ok(name), Ok(val)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            v.parse::<reqwest::header::HeaderValue>(),
        ) {
            h.insert(name, val);
        }
    }
    h
}

/// GET wham/usage for one account.
pub async fn fetch(
    hc: &reqwest::Client,
    root: &str,
    token: &str,
    account_id: &str,
    defaults: &std::collections::HashMap<String, String>,
) -> Result<Report, String> {
    let url = format!("{}/backend-api/wham/usage", root.trim_end_matches('/'));
    let res = hc
        .get(url)
        .headers(headers(token, account_id, defaults))
        .send()
        .await
        .map_err(|e| format!("usage probe: {e}"))?;
    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    if status != reqwest::StatusCode::OK {
        return Err(format!("usage endpoint {status}: {}", truncate(&body)));
    }
    parse(body.as_bytes())
}

/// GET wham/usage for one account, returning the raw upstream body (used to
/// serve the CLI's own usage poll verbatim).
pub async fn fetch_raw(
    hc: &reqwest::Client,
    root: &str,
    token: &str,
    account_id: &str,
    defaults: &std::collections::HashMap<String, String>,
) -> Result<String, String> {
    let url = format!("{}/backend-api/wham/usage", root.trim_end_matches('/'));
    let res = hc
        .get(url)
        .headers(headers(token, account_id, defaults))
        .send()
        .await
        .map_err(|e| format!("usage probe: {e}"))?;
    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    if status != reqwest::StatusCode::OK {
        return Err(format!("usage endpoint {status}: {}", truncate(&body)));
    }
    Ok(body)
}

/// GET the individual banked reset credits so callers can choose one safely.
pub async fn list_reset_credits(
    hc: &reqwest::Client,
    root: &str,
    token: &str,
    account_id: &str,
    defaults: &std::collections::HashMap<String, String>,
) -> Result<ResetCredits, String> {
    let url = format!(
        "{}/backend-api/wham/rate-limit-reset-credits",
        root.trim_end_matches('/')
    );
    let res = hc
        .get(url)
        .headers(headers(token, account_id, defaults))
        .send()
        .await
        .map_err(|e| format!("list reset credits: {e}"))?;
    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    if status != reqwest::StatusCode::OK {
        return Err(format!("list reset credits {status}: {}", truncate(&body)));
    }
    serde_json::from_str(&body).map_err(|e| format!("reset credits payload: {e}"))
}

/// Select the earliest-expiring available credit. Credits without an expiry
/// are valid but sort after every dated credit. Stable sorting preserves the
/// upstream order when two credits have the same expiry (or both never expire).
pub fn earliest_available_reset_credit(credits: &ResetCredits) -> Result<ResetCredit, String> {
    let mut available = credits
        .credits
        .iter()
        .filter(|credit| credit.status.eq_ignore_ascii_case("available"))
        .map(|credit| {
            let expiry = credit
                .expires_at
                .as_deref()
                .map(|raw| {
                    OffsetDateTime::parse(raw, &Rfc3339)
                        .map_err(|_| "invalid reset credit expiry timestamp".to_string())
                })
                .transpose()?;
            Ok((credit.clone(), expiry))
        })
        .collect::<Result<Vec<_>, String>>()?;
    available.sort_by(|(_, left), (_, right)| match (left, right) {
        (Some(left), Some(right)) => left.cmp(right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    });
    available
        .into_iter()
        .next()
        .map(|(credit, _)| credit)
        .ok_or_else(|| "no available reset credit".into())
}

/// POST consume one explicitly selected banked reset credit.
pub async fn consume_reset_credit(
    hc: &reqwest::Client,
    root: &str,
    token: &str,
    account_id: &str,
    defaults: &std::collections::HashMap<String, String>,
    credit_id: &str,
    redeem_request_id: &str,
) -> Result<(), String> {
    let url = format!(
        "{}/backend-api/wham/rate-limit-reset-credits/consume",
        root.trim_end_matches('/')
    );
    let payload = serde_json::json!({
        "credit_id": credit_id,
        "redeem_request_id": redeem_request_id,
    });
    let res = hc
        .post(url)
        .headers(headers(token, account_id, defaults))
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("consume: {e}"))?;
    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("consume {status}: {}", truncate(&body)));
    }
    Ok(())
}

fn truncate(s: &str) -> String {
    s.chars().take(200).collect()
}

/// Normalizes an additional limit name to a model key:
/// "GPT-5.3-Codex-Spark" -> "gpt-5.3-codex-spark".
pub fn model_key(limit_name: &str) -> String {
    limit_name.trim().to_lowercase().replace(' ', "-")
}

pub const CODEX_LATEST_RELEASE: &str = "https://releases.openai.com/codex/channels/latest";

#[derive(Deserialize)]
struct ReleaseMetadata {
    tag_name: String,
}

fn release_version(tag: &str) -> Result<String, String> {
    let version = tag
        .strip_prefix("rust-v")
        .ok_or("release tag is not a Codex version")?;
    let parts: Vec<_> = version.split('.').collect();
    if parts.len() != 3
        || parts.iter().any(|p| {
            p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || p.parse::<u32>().is_err()
        })
    {
        return Err("release tag is not a stable Codex version".into());
    }
    Ok(version.to_string())
}

/// Query the same stable release channel as the official Codex installer.
/// No config value or previous version is substituted on failure.
pub async fn fetch_latest_codex_version(hc: &reqwest::Client, url: &str) -> Result<String, String> {
    let metadata: ReleaseMetadata = hc
        .get(url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("release fetch: {e}"))?
        .error_for_status()
        .map_err(|e| format!("release fetch: {e}"))?
        .json()
        .await
        .map_err(|e| format!("release metadata: {e}"))?;
    release_version(&metadata.tag_name)
}

pub async fn fetch_model_slugs(
    hc: &reqwest::Client,
    root: &str,
    token: &str,
    account_id: &str,
    defaults: &std::collections::HashMap<String, String>,
    client_version: &str,
) -> Result<Vec<String>, String> {
    // Upstream tailors this catalog by client_version: older versions can
    // silently omit newly released models.
    let url = format!(
        "{}/models?client_version={}",
        root.trim_end_matches('/'),
        client_version
    );
    let mut h = headers(token, account_id, defaults);
    h.insert(
        "Version",
        client_version
            .parse()
            .map_err(|_| "invalid Codex version header")?,
    );
    h.insert(
        reqwest::header::USER_AGENT,
        format!("codex_cli_rs/{client_version}")
            .parse()
            .map_err(|_| "invalid Codex user-agent header")?,
    );
    let res = hc
        .get(url)
        .headers(h)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "models endpoint {status}: {}",
            &body[..body.len().min(160)]
        ));
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("models decode: {e}"))?;
    Ok(v.get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    m.get("slug")
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_string())
                })
                .collect()
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_tags_require_stable_codex_semver() {
        assert_eq!(release_version("rust-v0.159.2").unwrap(), "0.159.2");
        for tag in [
            "",
            "v0.159.2",
            "rust-v0.159",
            "rust-v0.159.2-beta",
            "rust-v0.159.2.3",
            "rust-v0.x.2",
        ] {
            assert!(release_version(tag).is_err(), "accepted {tag}");
        }
    }

    #[test]
    fn parses_wham_usage_payload() {
        let body = br#"{
            "plan_type": "pro",
            "rate_limit": {"allowed": true, "limit_reached": false,
                "primary_window": {"used_percent": 12, "limit_window_seconds": 18000, "reset_after_seconds": 3600},
                "secondary_window": {"used_percent": 44, "reset_at": 1893456000}},
            "additional_rate_limits": [{"limit_name": "GPT-5.3-Codex-Spark", "rate_limit": {
                "allowed": true, "primary_window": {"used_percent": 3}}}],
            "rate_limit_reset_credits": {"available_count": 2, "applicable_available_count": 2}
        }"#;
        let r = parse(body).unwrap();
        assert_eq!(r.plan_type, "pro");
        assert!(r.main.allowed);
        assert_eq!(r.main.primary.used_pct, Some(12.0));
        assert!(r.main.primary.reset_at.unwrap() > now_secs());
        assert_eq!(r.main.secondary.reset_at, Some(1893456000));
        assert_eq!(r.additional[0].name, "GPT-5.3-Codex-Spark");
        assert_eq!(r.additional[0].limit.primary.used_pct, Some(3.0));
        assert_eq!(r.credits.as_ref().unwrap().available, 2);
    }

    #[test]
    fn model_key_normalizes_limit_name() {
        assert_eq!(model_key("GPT-5.3-Codex-Spark"), "gpt-5.3-codex-spark");
    }

    #[test]
    fn selects_the_available_credit_with_the_earliest_expiry() {
        let credits: ResetCredits = serde_json::from_value(serde_json::json!({
            "available_count": 3,
            "credits": [
                {"id": "late", "status": "available", "expires_at": "2026-12-01T00:00:00Z"},
                {"id": "used", "status": "redeemed", "expires_at": "2026-10-01T00:00:00Z"},
                {"id": "early", "status": "available", "expires_at": "2026-10-15T00:00:00Z"},
                {"id": "never", "status": "available", "expires_at": null}
            ]
        }))
        .unwrap();
        assert_eq!(
            earliest_available_reset_credit(&credits).unwrap().id,
            "early"
        );
    }

    #[test]
    fn puts_never_expiring_credits_after_dated_credits_and_keeps_ties_stable() {
        let credits: ResetCredits = serde_json::from_value(serde_json::json!({
            "credits": [
                {"id": "never-first", "status": "available", "expires_at": null},
                {"id": "never-second", "status": "available", "expires_at": null},
                {"id": "dated", "status": "available", "expires_at": "2026-10-15T00:00:00Z"}
            ]
        }))
        .unwrap();
        assert_eq!(
            earliest_available_reset_credit(&credits).unwrap().id,
            "dated"
        );
        let credits: ResetCredits = serde_json::from_value(serde_json::json!({
            "credits": [
                {"id": "first", "status": "available", "expires_at": "2026-10-15T00:00:00Z"},
                {"id": "second", "status": "available", "expires_at": "2026-10-15T00:00:00Z"}
            ]
        }))
        .unwrap();
        assert_eq!(
            earliest_available_reset_credit(&credits).unwrap().id,
            "first"
        );
    }

    #[test]
    fn refuses_to_choose_when_an_available_expiry_is_unparseable() {
        let credits: ResetCredits = serde_json::from_value(serde_json::json!({
            "credits": [
                {"id": "bad", "status": "available", "expires_at": "not-a-date"}
            ]
        }))
        .unwrap();
        assert_eq!(
            earliest_available_reset_credit(&credits).unwrap_err(),
            "invalid reset credit expiry timestamp"
        );
    }

    #[tokio::test]
    async fn consume_sends_the_selected_credit_id() {
        use axum::routing::post;
        use axum::{Json, Router};
        use std::sync::{Arc, Mutex};

        let seen = Arc::new(Mutex::new(None));
        let receiver = seen.clone();
        let router = Router::new().route(
            "/backend-api/wham/rate-limit-reset-credits/consume",
            post(move |Json(body): Json<serde_json::Value>| {
                let receiver = receiver.clone();
                async move {
                    *receiver.lock().unwrap() = Some(body);
                    "{}"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        consume_reset_credit(
            &reqwest::Client::new(),
            &root,
            "test-token",
            "test-account",
            &std::collections::HashMap::new(),
            "credit-early",
            "redeem-test",
        )
        .await
        .unwrap();
        task.abort();
        assert_eq!(
            seen.lock().unwrap().as_ref().unwrap(),
            &serde_json::json!({
                "credit_id": "credit-early",
                "redeem_request_id": "redeem-test"
            })
        );
    }
}
