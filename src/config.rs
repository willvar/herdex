use serde::Deserialize;
use std::collections::HashMap;
use toml::Value;

#[derive(Deserialize, Clone, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct ManageCfg {
    #[serde(default)]
    pub key: String,
}

#[derive(Deserialize, Clone, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct OAuthCfg {
    #[serde(default)]
    pub issuer: String,
    #[serde(default)]
    pub client_id: String,
}

#[derive(Deserialize, Clone, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct UpstreamCfg {
    #[serde(default)]
    pub base_url: String,
}

#[derive(Deserialize, Clone, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct LogCfg {
    #[serde(default)]
    pub level: String,
}

#[derive(Deserialize, Clone, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct Config {
    #[serde(default)]
    pub listen: String,
    #[serde(default)]
    pub state_root: String,
    #[serde(default)]
    pub manage: ManageCfg,
    #[serde(default)]
    pub oauth: OAuthCfg,
    #[serde(default)]
    pub upstream: UpstreamCfg,
    #[serde(default)]
    pub header_defaults: HashMap<String, String>,
    #[serde(default)]
    pub log: LogCfg,
    /// request_log retention in days; missing -> 730 (2 years), 0 -> keep forever
    #[serde(default)]
    pub retention_days: Option<i64>,
    #[serde(default)]
    pub tls: TlsCfg,
    /// Optional override for /v1/models. Empty = serve the intersection of
    /// upstream-discovered catalogs (fallback: builtin catalog).
    #[serde(default)]
    pub models: Vec<String>,
}

/// Loads config with env interpolation (`env: NAME` mapping/scalar and
/// `{env:NAME}` scalar), applies defaults, and validates.
pub fn load(path: &str) -> Result<Config, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut value: Value = toml::from_str(&raw).map_err(|e| format!("parse {}: {e}", path))?;
    interpolate(&mut value);
    let mut cfg: Config =
        Config::deserialize(value).map_err(|e| format!("config {}: {e}", path))?;
    cfg.apply_defaults();
    cfg.validate()?;
    Ok(cfg)
}

/// Native TLS termination: a locally-generated private CA signs a leaf for
/// the configured hosts (IP SANs allowed), so codex can talk to a
/// domain-less LAN gateway over HTTPS with `CODEX_CA_CERTIFICATE`.
#[derive(Clone, Deserialize, Default)]
pub struct TlsCfg {
    /// DNS names / IPs the leaf certificate must cover; empty = auto
    /// (all local non-loopback IPs + hostname). TLS is unconditional:
    /// herdex serves codex, and codex 0.156+ only speaks HTTPS to its
    /// backend.
    #[serde(default)]
    pub hosts: Vec<String>,
}

impl TlsCfg {
    /// Empty `hosts` = zero-config: every local non-loopback IP plus the
    /// machine hostname is covered automatically. An explicit list is
    /// honored verbatim (localhost/127.0.0.1 always included).
    pub fn enabled_hosts(&self) -> Vec<String> {
        let mut hosts = if self.hosts.is_empty() {
            crate::tls::auto_hosts()
        } else {
            self.hosts
                .iter()
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
                .collect()
        };
        if !hosts.contains(&"localhost".to_string()) {
            hosts.push("localhost".into());
        }
        if !hosts.iter().any(|h| h == "127.0.0.1") {
            hosts.push("127.0.0.1".into());
        }
        hosts
    }
}

impl Config {
    pub fn retention_days(&self) -> i64 {
        self.retention_days.unwrap_or(730)
    }

    /// The codex client version we serve for — upstream endpoints tailor
    /// responses to it (the models catalog is empty for stale versions).
    pub fn client_version(&self) -> &str {
        self.header_defaults
            .get("version")
            .map(|s| s.as_str())
            .unwrap_or("0.156.1")
    }

    fn apply_defaults(&mut self) {
        if self.listen.is_empty() {
            self.listen = "127.0.0.1:8317".into();
        }
        if self.state_root.is_empty() {
            self.state_root = "/var/lib/herdex".into();
        }
        if self.oauth.issuer.is_empty() {
            self.oauth.issuer = "https://auth.openai.com".into();
        }
        if self.oauth.client_id.is_empty() {
            self.oauth.client_id = "app_EMoamEEZ73f0CkXaXp7hrann".into();
        }
        if self.upstream.base_url.is_empty() {
            self.upstream.base_url = "https://chatgpt.com/backend-api/codex".into();
        }
        if self.log.level.is_empty() {
            self.log.level = "info".into();
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.manage.key.is_empty() {
            return Err("manage.key: required (panel must never be unauthenticated)".into());
        }
        Ok(())
    }

    pub fn db_path(&self) -> String {
        format!("{}/herdex.db", self.state_root.trim_end_matches('/'))
    }
}

/// Walks the TOML tree rewriting `env = "NAME"` inline tables, `env: NAME`
/// scalars and `{env:NAME}` scalars into their environment lookups.
fn interpolate(v: &mut Value) {
    match v {
        Value::Table(m) => {
            let env_key = "env".to_string();
            if m.len() == 1 {
                if let Some(name) = m.get(&env_key) {
                    let name = name.as_str().unwrap_or("").trim().to_string();
                    *v = Value::String(std::env::var(&name).unwrap_or_default());
                    return;
                }
            }
            for (_k, val) in m.iter_mut() {
                interpolate(val);
            }
        }
        Value::Array(seq) => {
            for x in seq {
                interpolate(x);
            }
        }
        Value::String(s) => {
            let t = s.trim();
            if let Some(name) = t.strip_prefix("env: ") {
                *s = std::env::var(name.trim()).unwrap_or_default();
            } else if let Some(inner) = t.strip_prefix("{env:") {
                if let Some(name) = inner.strip_suffix('}') {
                    *s = std::env::var(name.trim()).unwrap_or_default();
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_example_keeps_retention_at_the_root() {
        let mut value: Value = toml::from_str(include_str!("../config.example.toml")).unwrap();
        interpolate(&mut value);
        let cfg = Config::deserialize(value).unwrap();
        assert_eq!(cfg.retention_days, Some(730));
        assert!(!cfg.header_defaults.contains_key("retention-days"));
    }

    fn write_tmp(body: &str) -> String {
        let path = std::env::temp_dir().join(format!(
            "herdex-cfg-{}-{}.toml",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::write(&path, body).unwrap();
        path.to_string_lossy().to_string()
    }

    fn rand_suffix() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64
    }

    #[test]
    fn defaults_applied() {
        let cfg = load(&write_tmp("[manage]\nkey = \"cpm-test\"\n")).unwrap();
        assert!(!cfg.listen.is_empty());
        assert!(!cfg.state_root.is_empty());
        assert!(!cfg.oauth.issuer.is_empty());
        assert!(!cfg.upstream.base_url.is_empty());
        assert_eq!(cfg.log.level, "info");
    }

    #[test]
    fn missing_manage_key_rejected() {
        assert!(load(&write_tmp("listen = \":0\"\n")).is_err());
    }

    #[test]
    fn env_interpolation_mapping_and_scalar() {
        std::env::set_var("HERDEX_TEST_KEY", "cpm-from-env");
        std::env::set_var("HERDEX_TEST_TWO", "two");
        let cfg = load(&write_tmp(
            "manage = { key = { env = \"HERDEX_TEST_KEY\" } }\noauth = { issuer = \"env: HERDEX_TEST_TWO\" }\n",
        ))
        .unwrap();
        assert_eq!(cfg.manage.key, "cpm-from-env");
        assert_eq!(cfg.oauth.issuer, "two");
    }

    #[test]
    fn full_overrides() {
        let cfg = load(&write_tmp(
            "listen = \"192.168.168.254:8317\"\nheader-defaults = { originator = \"codex_cli_rs\" }\n[manage]\nkey = \"cpm-x\"\n[oauth]\nissuer = \"https://fake.invalid\"\ncallback-port = 1456\n",
        ))
        .unwrap();
        assert_eq!(cfg.oauth.issuer, "https://fake.invalid");
        assert_eq!(
            cfg.header_defaults.get("originator").unwrap(),
            "codex_cli_rs"
        );
    }
}
