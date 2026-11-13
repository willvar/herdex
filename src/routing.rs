//! Bounded, volatile routing bindings. Upstream tokens never cross account ownership.
use http::{HeaderMap, HeaderValue};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const MAX_TOKEN_BYTES: usize = 8192;
const MAX_BINDINGS: usize = 256;
const IDLE_TTL: Duration = Duration::from_secs(30 * 60);

type Key = [u8; 32];

fn digest(parts: &[&str]) -> Key {
    let mut h = Sha256::new();
    for part in parts {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    h.finalize().into()
}

pub fn scope(api_key: &str, session: &str, model: &str, backend: &str) -> Key {
    digest(&[api_key, session, model, backend])
}

pub fn owner(id: &str, account: &str, credential: &str) -> Key {
    digest(&[id, account, credential])
}

fn token_key(scope: &Key, token: &str) -> Key {
    let mut h = Sha256::new();
    h.update(scope);
    h.update(token.as_bytes());
    h.finalize().into()
}

fn usable_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= MAX_TOKEN_BYTES && !token.contains(['\r', '\n'])
}

struct Binding {
    owner: Key,
    token: String,
    used: Instant,
}

#[derive(Default)]
pub struct RoutingBindings {
    entries: HashMap<Key, Binding>,
}

impl RoutingBindings {
    fn prune(&mut self) {
        self.entries
            .retain(|_, entry| entry.used.elapsed() < IDLE_TTL);
    }

    pub fn request_token(
        &mut self,
        scope: &Key,
        incoming: Option<&str>,
        owner: &Key,
    ) -> Option<String> {
        self.prune();
        let entry = self.entries.get_mut(&token_key(scope, incoming?))?;
        if entry.owner != *owner {
            return None;
        }
        entry.used = Instant::now();
        Some(entry.token.clone())
    }

    fn insert(&mut self, key: Key, owner: Key, token: &str) {
        if !self.entries.contains_key(&key) && self.entries.len() >= MAX_BINDINGS {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, value)| value.used)
                .map(|(key, _)| *key)
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            key,
            Binding {
                owner,
                token: token.to_owned(),
                used: Instant::now(),
            },
        );
    }

    // A client may keep its first token even after pool failover. Rebind that
    // client-side alias to the new account's first returned token, never forward
    // the old account's opaque value to the new owner.
    pub fn remember(
        &mut self,
        scope: &Key,
        incoming: Option<&str>,
        owner: &Key,
        returned: &str,
    ) -> Option<String> {
        if !usable_token(returned) {
            return None;
        }
        self.prune();
        let canonical = self
            .request_token(scope, incoming, owner)
            .unwrap_or_else(|| returned.to_owned());
        if let Some(token) = incoming {
            self.insert(token_key(scope, token), *owner, &canonical);
        }
        self.insert(token_key(scope, returned), *owner, &canonical);
        self.insert(token_key(scope, &canonical), *owner, &canonical);
        Some(canonical)
    }
}

fn one(headers: &HeaderMap, name: &str, limit: usize) -> Result<Option<HeaderValue>, &'static str> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err("duplicate routing header");
    }
    let text = value.to_str().map_err(|_| "invalid routing header")?;
    if text.is_empty() || text.len() > limit || text.trim() != text {
        return Err("invalid routing header");
    }
    Ok(Some(value.clone()))
}

pub fn normalize_headers(headers: &HeaderMap) -> Result<HeaderMap, &'static str> {
    let mut result = headers.clone();
    for (canonical, alias) in [("session-id", "session_id"), ("thread-id", "thread_id")] {
        let primary = one(headers, canonical, 128)?;
        let alternate = one(headers, alias, 128)?;
        if matches!((&primary, &alternate), (Some(a), Some(b)) if a != b) {
            return Err("conflicting routing headers");
        }
        result.remove(alias);
        if let Some(value) = primary.or(alternate) {
            result.insert(canonical, value);
        }
    }
    one(headers, "x-codex-turn-state", MAX_TOKEN_BYTES)?;
    one(headers, "x-client-request-id", 128)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_are_explicit_and_conflicts_or_duplicates_fail_closed() {
        let mut headers = HeaderMap::new();
        headers.insert("session_id", HeaderValue::from_static("session-a"));
        assert_eq!(
            normalize_headers(&headers).unwrap()["session-id"],
            "session-a"
        );
        headers.insert("session-id", HeaderValue::from_static("session-b"));
        assert!(normalize_headers(&headers).is_err());
        headers.insert("session-id", HeaderValue::from_static("session-a"));
        assert!(normalize_headers(&headers).is_ok());
        headers.append("session-id", HeaderValue::from_static("session-a"));
        assert!(normalize_headers(&headers).is_err());
    }

    #[test]
    fn first_state_is_scoped_to_caller_session_model_and_owner() {
        let scope_a = scope("key-a", "session-a", "model", "backend");
        let owner_a = owner("a", "account-a", "credential-a");
        let owner_b = owner("b", "account-b", "credential-b");
        let mut bindings = RoutingBindings::default();
        assert_eq!(
            bindings.request_token(&scope_a, Some("external-token"), &owner_a),
            None
        );
        assert_eq!(
            bindings.remember(&scope_a, None, &owner_a, "state-a"),
            Some("state-a".into())
        );
        assert_eq!(
            bindings.remember(&scope_a, Some("state-a"), &owner_a, "later-state"),
            Some("state-a".into())
        );
        assert_eq!(
            bindings.request_token(&scope_a, Some("state-a"), &owner_b),
            None
        );
        assert_eq!(
            bindings.remember(&scope_a, Some("state-a"), &owner_b, "state-b"),
            Some("state-b".into())
        );
        assert_eq!(
            bindings.request_token(&scope_a, Some("state-a"), &owner_b),
            Some("state-b".into())
        );
        assert_eq!(
            bindings.request_token(&scope_a, Some("state-a"), &owner_a),
            None
        );
        for other in [
            scope("key-b", "session-a", "model", "backend"),
            scope("key-a", "session-b", "model", "backend"),
            scope("key-a", "session-a", "other-model", "backend"),
        ] {
            assert_eq!(
                bindings.request_token(&other, Some("state-a"), &owner_b),
                None
            );
        }
        assert_eq!(
            bindings.request_token(
                &scope_a,
                Some("state-a"),
                &owner("b", "account-b", "refreshed")
            ),
            None
        );
    }

    #[test]
    fn bindings_expire_and_storage_is_bounded() {
        let scope = scope("key", "session", "model", "backend");
        let owner = owner("id", "account", "credential");
        let mut bindings = RoutingBindings::default();
        for i in 0..1000 {
            bindings.remember(&scope, None, &owner, &format!("state-{i}"));
        }
        assert!(bindings.entries.len() <= MAX_BINDINGS);
        for entry in bindings.entries.values_mut() {
            entry.used = Instant::now() - IDLE_TTL;
        }
        assert_eq!(
            bindings.request_token(&scope, Some("state-999"), &owner),
            None
        );
        assert!(bindings.entries.is_empty());
    }
}
