//! Zero-cost codex usage probes on chatgpt.com:
//!   GET  /backend-api/wham/usage                     — per-account quota windows
//!   GET  /backend-api/wham/rate-limit-reset-credits  — banked reset credits
//!   POST /backend-api/wham/rate-limit-reset-credits/consume — spend one credit
//! None of these consume model quota.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// POST consume: spends one banked reset credit for the account.
pub async fn consume_reset_credit(
    hc: &reqwest::Client,
    root: &str,
    token: &str,
    account_id: &str,
    defaults: &std::collections::HashMap<String, String>,
    redeem_request_id: &str,
) -> Result<(), String> {
    let url = format!(
        "{}/backend-api/wham/rate-limit-reset-credits/consume",
        root.trim_end_matches('/')
    );
    let payload = serde_json::json!({ "redeem_request_id": redeem_request_id });
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
