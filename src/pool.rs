//! Account selection state machine.
//!
//! Invariants inherited from the Go version (each one a lesson learned from
//! CLIProxyAPI's failure modes, locked by tests):
//!   - a quota failure cools ONE account for ONE model with a short in-memory
//!     TTL; it never produces a pool-wide blackout
//!   - when every candidate is cooling down, selection still returns the
//!     least-recently-failed candidates instead of erroring
//!   - when a model has no specific observation, the account-level "default"
//!     observation (from the zero-cost usage probe) influences ordering

use crate::store::{Account, Store};
#[cfg(test)]
use rand::RngCore;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Default, serde::Serialize)]
pub struct Quota {
    pub primary_pct: f64,
    pub secondary_pct: f64,
    pub primary_reset_at: i64,
    pub secondary_reset_at: i64,
    pub observed_at: i64,
}

#[derive(Clone, Copy, Default)]
struct Failure {
    until: i64,
    last_fail: i64,
}

#[derive(Clone)]
struct Pin {
    acc_id: String,
    model: String,
    until: i64,
}

/// affinity TTL: a codex session stays pinned to one account so upstream
/// prompt caches stay warm; sliding window, refreshed on every pinned request.
const AFFINITY_TTL: i64 = 3600;
const AFFINITY_MAX: usize = 1000;

type Now = Box<dyn Fn() -> i64 + Send + Sync>;

#[derive(Clone)]
pub struct Pool {
    shared: Arc<PoolShared>,
}

struct PoolShared {
    st: Store,
    now: Now,
    state: Mutex<PoolInner>,
}

#[derive(Default)]
struct PoolInner {
    quota: HashMap<(String, String), Quota>,
    failures: HashMap<(String, String), Failure>,
    last_used: HashMap<String, i64>,
    affinity: HashMap<String, Pin>,
}

impl Pool {
    pub fn new(st: Store) -> Self {
        Pool::with_now(st, Box::new(|| crate::store::now_secs()))
    }

    pub fn with_now(st: Store, now: Now) -> Self {
        Pool {
            shared: Arc::new(PoolShared {
                st,
                now,
                state: Mutex::new(PoolInner::default()),
            }),
        }
    }

    #[allow(dead_code)]
    fn now_secs(&self) -> i64 {
        (self.shared.now)()
    }

    pub fn observe(&self, acc_id: &str, model: &str, q: Quota) {
        let mut inner = self.shared.state.lock().unwrap();
        let mut q = q;
        q.observed_at = (self.shared.now)();
        inner.quota.insert((acc_id.to_string(), model.to_string()), q);
    }

    pub fn observation(&self, acc_id: &str, model: &str) -> Option<Quota> {
        self.shared.state.lock().unwrap().quota.get(&(acc_id.to_string(), model.to_string())).copied()
    }

    pub fn mark_used(&self, acc_id: &str) {
        self.shared.state.lock().unwrap().last_used.insert(acc_id.to_string(), (self.shared.now)());
    }

    /// Cools the account+model pair for the configured TTL. With zero TTL
    /// (cooldown disabled) it only records last_fail for ordering.
    pub fn mark_failure(&self, acc_id: &str, model: &str) {
        let ttl = self
            .shared
            .st
            .get_settings()
            .map(|s| s.cooldown_seconds)
            .unwrap_or(300);
        let mut inner = self.shared.state.lock().unwrap();
        let now = (self.shared.now)();
        let key = (acc_id.to_string(), model.to_string());
        let mut f = inner.failures.get(&key).copied().unwrap_or_default();
        f.last_fail = now;
        f.until = now + ttl;
        inner.failures.insert(key, f);
    }

    /// Returns accounts eligible for `model`, best first. Never returns an
    /// empty list while any enabled eligible account exists.
    pub fn candidates(&self, model: &str) -> Result<Vec<Account>, String> {
        let accounts = self.shared.st.list_accounts()?;
        let now = (self.shared.now)();
        let inner = self.shared.state.lock().unwrap();

        let mut active: Vec<Scored> = Vec::new();
        let mut cooled: Vec<Scored> = Vec::new();
        for a in accounts {
            if a.disabled {
                continue;
            }
            let key = (a.id.clone(), model.to_string());
            // fall back to the account-level "default" observation (from the
            // zero-cost usage probe) when the model has no specific one
            let q = inner
                .quota
                .get(&key)
                .or_else(|| inner.quota.get(&(a.id.clone(), "default".to_string())))
                .copied()
                .unwrap_or_default();
            let f = inner.failures.get(&key).copied().unwrap_or_default();
            let s = Scored {
                acc: a.clone(),
                primary: q.primary_pct,
                secondary: q.secondary_pct,
                last_used: inner.last_used.get(&a.id).copied().unwrap_or(0),
                last_failed: f.last_fail,
            };
            if f.until > now {
                cooled.push(s);
            } else {
                active.push(s);
            }
        }
        if active.is_empty() && !cooled.is_empty() {
            active = cooled; // never blackout: fall back to all
        }
        active.sort_by(|a, b| a.score(b));
        Ok(active.into_iter().map(|s| s.acc).collect())
    }

    /// Session affinity: the account a session is sticky to, if the pin is
    /// fresh and was made for the same model. Refreshes the sliding TTL.
    pub fn pinned(&self, session_key: &str, model: &str) -> Option<String> {
        if session_key.is_empty() {
            return None;
        }
        let mut inner = self.shared.state.lock().unwrap();
        let now = (self.shared.now)();
        match inner.affinity.get(session_key) {
            Some(pin) if pin.until > now && pin.model == model => {
                let mut p = pin.clone();
                p.until = now + AFFINITY_TTL;
                let acc_id = p.acc_id.clone();
                inner.affinity.insert(session_key.to_string(), p);
                Some(acc_id)
            }
            _ => {
                inner.affinity.remove(session_key);
                None
            }
        }
    }

    /// Sticks a session to the account that just served it successfully.
    pub fn pin(&self, session_key: &str, acc_id: &str, model: &str) {
        if session_key.is_empty() {
            return;
        }
        let mut inner = self.shared.state.lock().unwrap();
        let now = (self.shared.now)();
        inner.affinity.insert(
            session_key.to_string(),
            Pin { acc_id: acc_id.to_string(), model: model.to_string(), until: now + AFFINITY_TTL },
        );
        if inner.affinity.len() > AFFINITY_MAX {
            inner.affinity.retain(|_, p| p.until > now);
        }
    }

    /// All quota observations (accountID -> model -> Quota) for the panel.
    pub fn snapshot(&self) -> HashMap<String, HashMap<String, Quota>> {
        let inner = self.shared.state.lock().unwrap();
        let mut out: HashMap<String, HashMap<String, Quota>> = HashMap::new();
        for ((acc, model), q) in &inner.quota {
            out.entry(acc.clone()).or_default().insert(model.clone(), *q);
        }
        out
    }
}

#[derive(Clone)]
struct Scored {
    acc: Account,
    primary: f64,
    secondary: f64,
    last_used: i64,
    last_failed: i64,
}

impl Scored {
    /// Order by: least 5h usage, least weekly usage, LRU, least-recently-failed.
    fn score(&self, other: &Scored) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        self.primary
            .partial_cmp(&other.primary)
            .unwrap_or(Ordering::Equal)
            .then(self.secondary.partial_cmp(&other.secondary).unwrap_or(Ordering::Equal))
            .then(self.last_used.cmp(&other.last_used))
            .then(self.last_failed.cmp(&other.last_failed))
    }
}

/// model -> plans allowed to use it; models missing from the catalog are
#[cfg(test)]
mod tests {
    use super::*;
    
    fn test_pool() -> Pool {
        let dir = std::env::temp_dir().join(format!(
            "herdex-pool-{}-{}",
            std::process::id(),
            {
                let mut b = [0u8; 8];
                rand::thread_rng().fill_bytes(&mut b);
                u64::from_be_bytes(b)
            }
        ));
        let st = Store::open(dir.to_str().unwrap()).unwrap();
        Pool::new(st)
    }

    fn st_of(p: &Pool) -> Store {
        p.shared.st.clone()
    }

    fn seed(p: &Pool, ids: &[&str]) {
        let st = st_of(p);
        for id in ids {
            st.upsert_account(&Account {
                id: id.to_string(),
                email: format!("{id}@x"),
                plan_type: "pro".into(),
                access_token: format!("at-{id}"),
                refresh_token: format!("rt-{id}"),
                expires_at: 9999999999,
                ..Default::default()
            })
            .unwrap();
        }
    }

    #[test]
    fn least_used_ordering() {
        let p = test_pool();
        let st = st_of(&p);
        seed(&p, &["a1", "a2", "a3"]);
        p.observe("a1", "gpt-5.5", Quota { primary_pct: 80.0, secondary_pct: 90.0, ..Default::default() });
        p.observe("a2", "gpt-5.5", Quota { primary_pct: 10.0, secondary_pct: 20.0, ..Default::default() });

        let got = p.candidates("gpt-5.5").unwrap();
        let ids: Vec<&str> = got.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["a3", "a2", "a1"]);
    }

    #[test]
    fn failure_cooldown_never_blackout() {
        let dir = std::env::temp_dir().join(format!("herdex-pool-nb-{}", std::process::id()));
        let st = Store::open(dir.to_str().unwrap()).unwrap();
        let now_cell = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(1000));
        let now_ref = now_cell.clone();
        let p = Pool::with_now(st.clone(), Box::new(move || now_ref.load(std::sync::atomic::Ordering::SeqCst)));
        seed(&p, &["a1", "a2"]);

        p.mark_failure("a1", "gpt-5.5");
        p.mark_failure("a2", "gpt-5.5");
        // all cooled: must STILL return candidates
        let got = p.candidates("gpt-5.5").unwrap();
        assert_eq!(got.len(), 2);

        // TTL expiry restores normal ordering (now advances past cooldown)
        now_cell.store(now_cell.load(std::sync::atomic::Ordering::SeqCst) + 7200, std::sync::atomic::Ordering::SeqCst);
        p.observe("a1", "gpt-5.5", Quota { primary_pct: 5.0, ..Default::default() });
        let got = p.candidates("gpt-5.5").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "a2");
        let _ = st;
    }

    #[test]
    fn disabled_account_filtered() {
        let p = test_pool();
        let st = st_of(&p);
        seed(&p, &["a1"]);
        st.set_account_disabled("a1", true).unwrap();
        assert!(p.candidates("gpt-5.5").unwrap().is_empty());
    }

    #[test]
    fn unknown_model_allowed_everywhere() {
        let p = test_pool();
        let st = st_of(&p);
        seed(&p, &["plus1"]);
        assert_eq!(p.candidates("gpt-brand-new-model").unwrap().len(), 1);
    }

    #[test]
    fn session_affinity() {
        let dir = std::env::temp_dir().join(format!("herdex-pool-af-{}", std::process::id()));
        let st = Store::open(dir.to_str().unwrap()).unwrap();
        let now_cell = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(1000));
        let now_ref = now_cell.clone();
        let p = Pool::with_now(st, Box::new(move || now_ref.load(std::sync::atomic::Ordering::SeqCst)));
        seed(&p, &["a1"]);

        p.pin("sess-1", "a1", "gpt-5.5");
        assert_eq!(p.pinned("sess-1", "gpt-5.5").as_deref(), Some("a1"));
        // model mismatch drops the pin
        assert!(p.pinned("sess-1", "gpt-6-astra").is_none());
        // expiry drops the pin
        p.pin("sess-2", "a1", "gpt-5.5");
        now_cell.store(now_cell.load(std::sync::atomic::Ordering::SeqCst) + 7200, std::sync::atomic::Ordering::SeqCst);
        assert!(p.pinned("sess-2", "gpt-5.5").is_none());
    }

    #[test]
    fn default_observation_fallback_in_ordering() {
        let p = test_pool();
        let st = st_of(&p);
        seed(&p, &["a1", "a2"]);
        p.observe("a1", "default", Quota { primary_pct: 90.0, ..Default::default() });
        p.observe("a2", "default", Quota { primary_pct: 5.0, ..Default::default() });
        let got = p.candidates("gpt-5.6-sol").unwrap();
        assert_eq!(got[0].id, "a2");
    }
}
