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
    /// Internal marker for reports whose upstream payload only supplied a
    /// relative reset countdown. It must not be serialized as public usage
    /// data because the absolute `reset_at` above remains the API contract.
    #[serde(skip)]
    pub reset_after_seconds: Option<i64>,
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

/// A banked reset returned by the detail endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResetCredit {
    pub id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, alias = "grantedAt")]
    pub granted_at: Option<String>,
    #[serde(default, alias = "expiresAt")]
    pub expires_at: Option<String>,
    #[serde(default, alias = "resetType")]
    pub reset_type: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResetCredits {
    #[serde(default)]
    pub credits: Option<Vec<ResetCredit>>,
    #[serde(default, alias = "availableCount")]
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
    /// Detailed banked reset rows fetched from the dedicated endpoint by the
    /// management quota route. Normal background usage probes leave this
    /// empty so they do not add another upstream request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_credits: Option<ResetCredits>,
}

/// A complete quota window that is currently exhausted.  The scope is either
/// `main` or the normalized name of an additional model-specific limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExhaustedWindow {
    pub scope: String,
    pub slot: String,
    pub reset_at: i64,
    pub window_seconds: i64,
    pub reset_after_seconds: i64,
}

impl ExhaustedWindow {
    pub fn key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.scope, self.slot, self.reset_at, self.window_seconds
        )
    }
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
        reset_after_seconds: w.reset_after_seconds.filter(|after| *after > 0),
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
        reset_credits: None,
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

fn exhausted_window(scope: &str, slot: &str, window: &Window) -> Option<ExhaustedWindow> {
    let used_pct = window.used_pct?;
    if !used_pct.is_finite() || used_pct < 100.0 {
        return None;
    }
    let reset_at = window.reset_at.unwrap_or(0);
    let window_seconds = window.window_seconds.unwrap_or(0);
    // A percentage without a usable window identity can be a partial or
    // stale upstream payload; it is not safe to spend an irreversible card.
    if reset_at <= 0 && window_seconds <= 0 {
        return None;
    }
    Some(ExhaustedWindow {
        scope: scope.to_owned(),
        slot: slot.to_owned(),
        reset_at,
        window_seconds,
        reset_after_seconds: window.reset_after_seconds.unwrap_or(0),
    })
}

impl ExhaustedWindow {
    /// Relative reset countdowns are recalculated on every usage fetch, so
    /// their derived absolute timestamps are not stable episode identities.
    pub fn same_episode(&self, other: &Self) -> bool {
        if self.scope != other.scope || self.slot != other.slot {
            return false;
        }
        if self.window_seconds > 0
            && other.window_seconds > 0
            && self.window_seconds != other.window_seconds
        {
            return false;
        }
        if self.reset_after_seconds > 0 || other.reset_after_seconds > 0 {
            true
        } else {
            self.reset_at == other.reset_at
        }
    }
}

fn scoped_window<'a>(report: &'a Report, scope: &str, slot: &str) -> Option<&'a Window> {
    let limit = if scope == "main" {
        Some(&report.main)
    } else {
        report
            .additional
            .iter()
            .find(|a| model_key(&a.name) == scope)
            .map(|a| &a.limit)
    }?;
    match slot {
        "primary" => Some(&limit.primary),
        "secondary" => Some(&limit.secondary),
        _ => None,
    }
}

fn window_is_known(window: &Window) -> bool {
    window.used_pct.filter(|pct| pct.is_finite()).is_some()
        && (window.reset_at.unwrap_or(0) > 0 || window.window_seconds.unwrap_or(0) > 0)
}

fn window_is_present(window: &Window) -> bool {
    window.used_pct.is_some() || window.reset_at.is_some() || window.window_seconds.is_some()
}

/// Whether the usage report contains enough complete window metadata to make
/// an automatic reset decision for this model. Partial upstream payloads are
/// deliberately treated as unknown so they cannot cause a card spend or a
/// same-account replay.
pub fn quota_windows_known_for_model(report: &Report, model: &str) -> bool {
    let mut limits = vec![&report.main];
    let model = model_key(model);
    limits.extend(
        report
            .additional
            .iter()
            .filter(|a| model_key(&a.name) == model)
            .map(|a| &a.limit),
    );
    limits.iter().all(|limit| {
        let windows = [&limit.primary, &limit.secondary];
        windows.iter().any(|window| window_is_present(window))
            && windows
                .iter()
                .filter(|window| window_is_present(window))
                .all(|window| window_is_known(window))
    })
}

/// Returns the first complete exhausted window relevant to one requested
/// model. The main account limit is always relevant; model-specific limits are
/// considered when the upstream names one for this model.
pub fn exhausted_window_for_model(report: &Report, model: &str) -> Option<ExhaustedWindow> {
    let mut candidates = Vec::new();
    for (slot, window) in [
        ("primary", &report.main.primary),
        ("secondary", &report.main.secondary),
    ] {
        if let Some(exhausted) = exhausted_window("main", slot, window) {
            candidates.push(exhausted);
        }
    }
    let model = model_key(model);
    for additional in report
        .additional
        .iter()
        .filter(|a| model_key(&a.name) == model)
    {
        for (slot, window) in [
            ("primary", &additional.limit.primary),
            ("secondary", &additional.limit.secondary),
        ] {
            if let Some(exhausted) = exhausted_window(&model, slot, window) {
                candidates.push(exhausted);
            }
        }
    }
    candidates.into_iter().next()
}

/// Tells the reset coordinator that a previously exhausted window has either
/// recovered or rolled into a different upstream window.
pub fn window_recovered(report: &Report, episode: &ExhaustedWindow) -> bool {
    let Some(window) = scoped_window(report, &episode.scope, &episode.slot) else {
        return false;
    };
    if window
        .window_seconds
        .zip((episode.window_seconds > 0).then_some(episode.window_seconds))
        .is_some_and(|(current, previous)| current > 0 && current != previous)
    {
        return true;
    }
    // A reset_at derived from reset_after_seconds moves on every fetch and
    // cannot identify a new window. Only compare absolute timestamps when
    // both observations came from an explicit upstream reset_at.
    if window.reset_after_seconds.is_none()
        && episode.reset_after_seconds <= 0
        && window
            .reset_at
            .is_some_and(|reset_at| reset_at > 0 && reset_at != episode.reset_at)
    {
        return true;
    }
    window
        .used_pct
        .filter(|pct| pct.is_finite())
        .is_some_and(|pct| pct < 100.0)
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
        .as_deref()
        .unwrap_or(&[])
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

pub async fn fetch_model_catalog(
    hc: &reqwest::Client,
    root: &str,
    token: &str,
    account_id: &str,
    defaults: &std::collections::HashMap<String, String>,
    client_version: &str,
) -> Result<Vec<serde_json::Value>, String> {
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
    let models = v
        .get("models")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "models response has no model array".to_owned())?;
    if models.iter().any(|model| {
        model
            .get("slug")
            .and_then(serde_json::Value::as_str)
            .is_none_or(str::is_empty)
    }) {
        return Err("models response contains an invalid model entry".into());
    }
    Ok(models.clone())
}

pub async fn fetch_model_slugs(
    hc: &reqwest::Client,
    root: &str,
    token: &str,
    account_id: &str,
    defaults: &std::collections::HashMap<String, String>,
    client_version: &str,
) -> Result<Vec<String>, String> {
    Ok(
        fetch_model_catalog(hc, root, token, account_id, defaults, client_version)
            .await?
            .into_iter()
            .filter_map(|model| {
                model
                    .get("slug")
                    .and_then(|slug| slug.as_str())
                    .map(ToOwned::to_owned)
            })
            .collect(),
    )
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
    fn finds_only_complete_exhausted_windows_for_a_model() {
        let report = parse(
            br#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 42,
                        "limit_window_seconds": 604800,
                        "reset_at": 1893456000
                    }
                },
                "additional_rate_limits": [{
                    "limit_name": "GPT-5.3-Codex-Spark",
                    "rate_limit": {
                        "primary_window": {
                            "used_percent": 100,
                            "limit_window_seconds": 18000,
                            "reset_at": 1893456000
                        }
                    }
                }]
            }"#,
        )
        .unwrap();
        assert!(exhausted_window_for_model(&report, "gpt-5.5").is_none());
        let exhausted = exhausted_window_for_model(&report, "gpt-5.3-codex-spark").unwrap();
        assert_eq!(exhausted.scope, "gpt-5.3-codex-spark");
        assert_eq!(exhausted.slot, "primary");
    }

    #[test]
    fn recovery_requires_the_same_window_to_have_capacity_again() {
        let report = parse(
            br#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 42,
                        "limit_window_seconds": 604800,
                        "reset_at": 1893456000
                    }
                }
            }"#,
        )
        .unwrap();
        let episode = ExhaustedWindow {
            scope: "main".into(),
            slot: "primary".into(),
            reset_at: 1893456000,
            window_seconds: 604800,
            reset_after_seconds: 0,
        };
        assert!(window_recovered(&report, &episode));
    }

    #[test]
    fn relative_reset_countdowns_do_not_create_new_episodes_on_each_fetch() {
        let mut first = parse(
            br#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 100,
                        "limit_window_seconds": 604800,
                        "reset_after_seconds": 3600
                    }
                }
            }"#,
        )
        .unwrap();
        let episode = exhausted_window_for_model(&first, "gpt-5.5").unwrap();
        first.main.primary.reset_at = episode.reset_at.checked_add(10);
        first.main.primary.reset_after_seconds = Some(3590);
        let current = exhausted_window_for_model(&first, "gpt-5.5").unwrap();
        assert!(episode.same_episode(&current));
        assert!(!window_recovered(&first, &episode));
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
    fn preserves_reset_credit_grant_and_expiry_details() {
        let credits: ResetCredits = serde_json::from_value(serde_json::json!({
            "available_count": 1,
            "credits": [{
                "id": "credit-1",
                "status": "available",
                "granted_at": "2026-10-01T12:34:56Z",
                "expires_at": "2026-10-31T12:34:56Z"
            }]
        }))
        .unwrap();
        let credit = &credits.credits.as_ref().unwrap()[0];
        assert_eq!(credit.granted_at.as_deref(), Some("2026-10-01T12:34:56Z"));
        assert_eq!(credit.expires_at.as_deref(), Some("2026-10-31T12:34:56Z"));
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
