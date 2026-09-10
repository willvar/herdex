//! Codex CLI OAuth (PKCE) flow: authorize URL, code exchange, refresh, and
//! id_token claim extraction (including the nested OpenAI auth namespace).

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

#[derive(Deserialize, Clone, Default)]
pub struct TokenSet {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub expires_in: i64,
}

#[derive(Deserialize, Clone, Default)]
pub struct OpenAIAuth {
    #[serde(default)]
    pub chatgpt_account_id: String,
    #[serde(default)]
    pub chatgpt_plan_type: String,
}

#[derive(Deserialize, Clone, Default)]
pub struct Claims {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub exp: i64,
    #[serde(default)]
    pub chatgpt_account_id: String,
    #[serde(default)]
    pub chatgpt_plan_type: String,
    #[serde(default, rename = "https://api.openai.com/auth")]
    pub openai_auth: OpenAIAuth,
}

impl Claims {
    pub fn account_id(&self) -> String {
        if !self.chatgpt_account_id.is_empty() {
            self.chatgpt_account_id.clone()
        } else {
            self.openai_auth.chatgpt_account_id.clone()
        }
    }

    pub fn plan_type(&self) -> String {
        if !self.chatgpt_plan_type.is_empty() {
            self.chatgpt_plan_type.clone()
        } else {
            self.openai_auth.chatgpt_plan_type.clone()
        }
    }
}

#[derive(Clone)]
pub struct PKCE {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

pub fn new_pkce() -> Result<PKCE, String> {
    let verifier = random_b64(64)?;
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(digest);
    let state = random_b64(16)?;
    Ok(PKCE { verifier, challenge, state })
}

pub fn auth_url(issuer: &str, client_id: &str, redirect_uri: &str, p: &PKCE) -> String {
    let mut q = HashMap::new();
    q.insert("response_type", "code");
    q.insert("client_id", client_id);
    q.insert("redirect_uri", redirect_uri);
    q.insert("scope", "openid profile email offline_access");
    q.insert("state", &p.state);
    q.insert("code_challenge", &p.challenge);
    q.insert("code_challenge_method", "S256");
    q.insert("prompt", "login");
    q.insert("originator", "codex_cli_rs");
    let qs: Vec<String> = q
        .iter()
        .map(|(k, v)| format!("{}={}", k, url_encode(v)))
        .collect();
    format!("{}/oauth/authorize?{}", issuer.trim_end_matches('/'), qs.join("&"))
}

pub async fn exchange(
    hc: &reqwest::Client,
    issuer: &str,
    client_id: &str,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<TokenSet, String> {
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
    ];
    token_request(hc, issuer, &form).await
}

pub async fn refresh(
    hc: &reqwest::Client,
    issuer: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenSet, String> {
    let form = [
        ("grant_type", "refresh_token"),
        ("client_id", client_id),
        ("refresh_token", refresh_token),
    ];
    token_request(hc, issuer, &form).await
}

async fn token_request(hc: &reqwest::Client, issuer: &str, form: &[(&str, &str)]) -> Result<TokenSet, String> {
    let url = format!("{}/oauth/token", issuer.trim_end_matches('/'));
    let res = hc
        .post(url)
        .form(form)
        .send()
        .await
        .map_err(|e| format!("token endpoint: {e}"))?;
    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    if status != reqwest::StatusCode::OK {
        return Err(format!("token endpoint {status}: {}", truncate(&body)));
    }
    let ts: TokenSet = serde_json::from_str(&body).map_err(|e| format!("token response: {e}"))?;
    if ts.access_token.is_empty() || ts.refresh_token.is_empty() {
        return Err("token endpoint returned no tokens".into());
    }
    Ok(ts)
}

/// Decodes JWT claims without signature verification (tokens arrive over TLS
/// directly from the issuer; herdex never forwards them elsewhere).
pub fn parse_id_token(id_token: &str) -> Result<Claims, String> {
    let parts: Vec<&str> = id_token.split('.').collect();
    if parts.len() != 3 {
        return Err(format!("id_token: want 3 segments, got {}", parts.len()));
    }
    let payload = URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|e| format!("id_token payload: {e}"))?;
    serde_json::from_slice(&payload).map_err(|e| format!("id_token claims: {e}"))
}

fn random_b64(n: usize) -> Result<String, String> {
    use rand::RngCore;
    let mut buf = vec![0u8; n];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

pub fn b64_encode(data: &[u8]) -> String {
    STANDARD.encode(data)
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn truncate(s: &str) -> String {
    s.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_generated() {
        let p = new_pkce().unwrap();
        assert!(!p.verifier.is_empty() && !p.challenge.is_empty() && !p.state.is_empty());
        let other = new_pkce().unwrap();
        assert_ne!(p.state, other.state);
    }

    #[test]
    fn auth_url_contains_essentials() {
        let p = new_pkce().unwrap();
        let u = auth_url("https://auth.openai.com", "cid", "http://localhost:1455/auth/callback", &p);
        assert!(u.starts_with("https://auth.openai.com/oauth/authorize?"));
        assert!(u.contains("client_id=cid"));
        assert!(u.contains("code_challenge_method=S256"));
        assert!(u.contains("originator=codex_cli_rs"));
    }

    #[test]
    fn parses_nested_openai_auth_claims() {
        let payload = serde_json::json!({
            "email": "n@b.c",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acc-n",
                "chatgpt_plan_type": "prolite"
            },
            "exp": 1893456000
        });
        let tok = format!(
            "hdr.{}.sig",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        );
        let c = parse_id_token(&tok).unwrap();
        assert_eq!(c.account_id(), "acc-n");
        assert_eq!(c.plan_type(), "prolite");
    }

    #[test]
    fn parses_top_level_claims() {
        let payload = serde_json::json!({
            "email": "a@b.c", "chatgpt_plan_type": "pro",
            "chatgpt_account_id": "acc-1", "exp": 1893456000
        });
        let tok = format!(
            "hdr.{}.sig",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        );
        let c = parse_id_token(&tok).unwrap();
        assert_eq!(c.plan_type(), "pro");
        assert_eq!(c.account_id(), "acc-1");
    }

    #[test]
    fn malformed_rejected() {
        assert!(parse_id_token("not-a-jwt").is_err());
        assert!(parse_id_token("a.b.c").is_err());
    }
}
