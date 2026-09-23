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
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Default, serde::Serialize)]
pub struct Quota {
    pub primary_pct: f64,
    pub secondary_pct: f64,
    pub primary_reset_at: i64,
    pub secondary_reset_at: i64,
    /// window lengths in seconds, for labeling (18000=5h, 604800=weekly);
    /// upstream generalizes these — never assume 5h/weekly by position
    pub primary_window_secs: i64,
    pub secondary_window_secs: i64,
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
    /// Shadow ledger: tokens attributed locally since the account's last
    /// quota snapshot, bucketed per model. Upstream wham reports lag heavy
    /// sessions; without this the scheduler chases stale "least used"
    /// numbers and overloads the account that is actually burning fastest.
    pending: HashMap<String, HashMap<String, f64>>,
    /// Calibration cache (per-model tok/pp), refreshed lazily — the replay
    /// is too heavy for the per-request path.
    rates: HashMap<String, (i64, RateSet)>,
}

#[derive(Clone)]
struct RateSet {
    blended: f64,
    per_model: BTreeMap<String, f64>,
}

impl Pool {
    pub fn new(st: Store) -> Self {
        Pool::with_now(st, Box::new(crate::store::now_secs))
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
        let mut inner = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut q = q;
        q.observed_at = (self.shared.now)();
        inner
            .quota
            .insert((acc_id.to_string(), model.to_string()), q);
        // reconcile: the fresh snapshot already includes whatever we burned
        // since the previous one — holding the shadow estimate any longer
        // would double-count it
        inner.pending.remove(acc_id);
        inner.rates.remove(acc_id);
    }

    /// Records tokens a completed request attributed to this account
    /// (shadow ledger). The pool's quota snapshots only refresh on the
    /// usage poll; between polls these tokens are invisible to scheduling
    /// unless priced in here.
    pub fn accrue(&self, acc_id: &str, model: &str, tokens: i64) {
        if tokens <= 0 {
            return;
        }
        let mut inner = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let bucket = inner.pending.entry(acc_id.to_string()).or_default();
        *bucket.entry(model.to_string()).or_insert(0.0) += tokens as f64;
    }

    /// Unburned pp implied by locally-attributed tokens not yet reflected
    /// in the account's quota snapshot (capped at 100).
    fn pending_pp(store: &Store, inner: &mut PoolInner, acc_id: &str, now: i64) -> f64 {
        let Some(bucket) = inner.pending.get(acc_id).cloned() else {
            return 0.0;
        };
        let rate = Self::rate(store, inner, acc_id, now);
        let Some(rate) = rate else { return 0.0 };
        let pp: f64 = bucket
            .iter()
            .map(|(m, tokens)| tokens / rate.per_model.get(m).copied().unwrap_or(rate.blended))
            .sum();
        pp.min(100.0)
    }

    fn rate(store: &Store, inner: &mut PoolInner, acc_id: &str, now: i64) -> Option<RateSet> {
        const TTL: i64 = 60;
        if let Some((fetched, rate)) = inner.rates.get(acc_id) {
            if now - *fetched < TTL {
                return Some(rate.clone());
            }
        }
        let cal = store.calibration(acc_id).ok().flatten()?;
        let entry = RateSet {
            blended: cal.tokens_per_pct,
            per_model: cal.per_model.clone(),
        };
        inner.rates.insert(acc_id.to_string(), (now, entry.clone()));
        Some(entry)
    }

    pub fn observation(&self, acc_id: &str, model: &str) -> Option<Quota> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .quota
            .get(&(acc_id.to_string(), model.to_string()))
            .copied()
    }

    pub fn mark_used(&self, acc_id: &str) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_used
            .insert(acc_id.to_string(), (self.shared.now)());
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
        let mut inner = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
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
        Ok(self
            .scored_candidates(model)?
            .into_iter()
            .map(|s| s.acc)
            .collect())
    }

    /// Ordered candidates for a request, with session affinity: the session's
    /// pinned account is moved to the front unless its effective score lags
    /// the best candidate by more than 20pp (then the pin is left to expire).
    pub fn select(&self, model: &str, session_key: &str) -> Result<Vec<Account>, String> {
        let gap = self
            .shared
            .st
            .get_settings()
            .map(|s| s.pin_yield_gap_pp)
            .unwrap_or(20);
        let mut scored = self.scored_candidates(model)?;
        if !session_key.is_empty() {
            if let Some(pin) = self.pinned(session_key, model) {
                if let Some(i) = scored.iter().position(|s| s.acc.id == pin) {
                    // -1 = sticky-first (pin rides in front, migrate only on
                    // failure); 0 = water-filling chase (always ride the
                    // least-used account); >0 = hysteresis band
                    let keep = if gap < 0 {
                        true
                    } else {
                        scored[i].effective() <= scored[0].effective() + gap as f64
                    };
                    if keep && i != 0 {
                        scored.swap(0, i);
                    }
                }
            }
        }
        Ok(scored.into_iter().map(|s| s.acc).collect())
    }

    fn scored_candidates(&self, model: &str) -> Result<Vec<Scored>, String> {
        let accounts = self.shared.st.list_accounts()?;
        let now = (self.shared.now)();
        let mut inner = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());

        let mut active: Vec<Scored> = Vec::new();
        let mut cooled: Vec<Scored> = Vec::new();
        for a in accounts {
            if a.disabled {
                continue;
            }
            let key = (a.id.clone(), model.to_string());
            // model-specific observation when present; the account-level
            // "default" observation (main quota window) always participates —
            // a 0% model window must not mask an almost-exhausted account pool
            let qm = inner.quota.get(&key).copied();
            let qd = inner
                .quota
                .get(&(a.id.clone(), "default".to_string()))
                .copied();
            let primary = match (qm, qd) {
                (Some(m), Some(d)) => m.primary_pct.max(d.primary_pct),
                (Some(m), None) => m.primary_pct,
                (None, Some(d)) => d.primary_pct,
                (None, None) => 0.0,
            };
            let secondary = match (qm, qd) {
                (Some(m), Some(d)) => m.secondary_pct.max(d.secondary_pct),
                (Some(m), None) => m.secondary_pct,
                (None, Some(d)) => d.secondary_pct,
                (None, None) => 0.0,
            };
            let f = inner.failures.get(&key).copied().unwrap_or_default();
            // shadow ledger: the snapshot may be minutes stale during heavy
            // sessions — add the locally-attributed, not-yet-reported burn
            // so "least used" reflects reality, then never lower what the
            // snapshot says (only upward correction, per the reconciliation
            // contract with observe())
            let burn = Pool::pending_pp(&self.shared.st, &mut inner, &a.id, now);
            let s = Scored {
                acc: a.clone(),
                primary: primary + burn,
                secondary,
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
        Ok(active)
    }

    /// Hydrates the in-memory quota map from the latest persisted probes —
    /// a restart would otherwise blank the capacity panel until the first
    /// upstream usage poll. Stale values are corrected by the next poll.
    pub fn hydrate_from_store(&self) {
        let Ok(latest) = self.shared.st.latest_probes() else {
            return;
        };
        let mut inner = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = (self.shared.now)();
        for (acc_id, pct, reset_at, ts) in latest {
            let key = (acc_id.clone(), "default".to_string());
            if let Some(existing) = inner.quota.get(&key) {
                if existing.observed_at >= now - 60 {
                    continue; // a fresh poll already superseded the probe
                }
            }
            inner.quota.insert(
                key,
                Quota {
                    primary_pct: pct,
                    secondary_pct: pct,
                    primary_reset_at: reset_at,
                    secondary_reset_at: reset_at,
                    primary_window_secs: 0,
                    secondary_window_secs: 0,
                    observed_at: ts,
                },
            );
        }
    }

    /// Session affinity: the account a session is sticky to, if the pin is
    /// fresh and was made for the same model. Refreshes the sliding TTL.
    pub fn pinned(&self, session_key: &str, model: &str) -> Option<String> {
        if session_key.is_empty() {
            return None;
        }
        let mut inner = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
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
        let mut inner = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = (self.shared.now)();
        inner.affinity.insert(
            session_key.to_string(),
            Pin {
                acc_id: acc_id.to_string(),
                model: model.to_string(),
                until: now + AFFINITY_TTL,
            },
        );
        if inner.affinity.len() > AFFINITY_MAX {
            inner.affinity.retain(|_, p| p.until > now);
        }
    }

    /// All quota observations (accountID -> model -> Quota) for the panel.
    pub fn snapshot(&self) -> HashMap<String, HashMap<String, Quota>> {
        let inner = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: HashMap<String, HashMap<String, Quota>> = HashMap::new();
        for ((acc, model), q) in &inner.quota {
            out.entry(acc.clone())
                .or_default()
                .insert(model.clone(), *q);
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
    /// Effective scarcity: the tighter of primary/secondary window usage.
    fn effective(&self) -> f64 {
        self.primary.max(self.secondary)
    }

    /// Order by: least effective usage, LRU, least-recently-failed.
    fn score(&self, other: &Scored) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        self.effective()
            .partial_cmp(&other.effective())
            .unwrap_or(Ordering::Equal)
            .then(self.last_used.cmp(&other.last_used))
            .then(self.last_failed.cmp(&other.last_failed))
    }
}

/// model -> plans allowed to use it; models missing from the catalog are
#[cfg(test)]
mod tests {
    use super::*;

    fn test_pool() -> Pool {
        let dir = std::env::temp_dir().join(format!("herdex-pool-{}-{}", std::process::id(), {
            let mut b = [0u8; 8];
            rand::thread_rng().fill_bytes(&mut b);
            u64::from_be_bytes(b)
        }));
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
        let _st = st_of(&p);
        seed(&p, &["a1", "a2", "a3"]);
        p.observe(
            "a1",
            "gpt-5.5",
            Quota {
                primary_pct: 80.0,
                secondary_pct: 90.0,
                ..Default::default()
            },
        );
        p.observe(
            "a2",
            "gpt-5.5",
            Quota {
                primary_pct: 10.0,
                secondary_pct: 20.0,
                ..Default::default()
            },
        );

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
        let p = Pool::with_now(
            st.clone(),
            Box::new(move || now_ref.load(std::sync::atomic::Ordering::SeqCst)),
        );
        seed(&p, &["a1", "a2"]);

        p.mark_failure("a1", "gpt-5.5");
        p.mark_failure("a2", "gpt-5.5");
        // all cooled: must STILL return candidates
        let got = p.candidates("gpt-5.5").unwrap();
        assert_eq!(got.len(), 2);

        // TTL expiry restores normal ordering (now advances past cooldown)
        now_cell.store(
            now_cell.load(std::sync::atomic::Ordering::SeqCst) + 7200,
            std::sync::atomic::Ordering::SeqCst,
        );
        p.observe(
            "a1",
            "gpt-5.5",
            Quota {
                primary_pct: 5.0,
                ..Default::default()
            },
        );
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
        let _st = st_of(&p);
        seed(&p, &["plus1"]);
        assert_eq!(p.candidates("gpt-brand-new-model").unwrap().len(), 1);
    }

    #[test]
    fn session_affinity() {
        let dir = std::env::temp_dir().join(format!("herdex-pool-af-{}", std::process::id()));
        let st = Store::open(dir.to_str().unwrap()).unwrap();
        let now_cell = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(1000));
        let now_ref = now_cell.clone();
        let p = Pool::with_now(
            st,
            Box::new(move || now_ref.load(std::sync::atomic::Ordering::SeqCst)),
        );
        seed(&p, &["a1"]);

        p.pin("sess-1", "a1", "gpt-5.5");
        assert_eq!(p.pinned("sess-1", "gpt-5.5").as_deref(), Some("a1"));
        // model mismatch drops the pin
        assert!(p.pinned("sess-1", "gpt-6-astra").is_none());
        // expiry drops the pin
        p.pin("sess-2", "a1", "gpt-5.5");
        now_cell.store(
            now_cell.load(std::sync::atomic::Ordering::SeqCst) + 7200,
            std::sync::atomic::Ordering::SeqCst,
        );
        assert!(p.pinned("sess-2", "gpt-5.5").is_none());
    }

    #[test]
    fn default_observation_fallback_in_ordering() {
        let p = test_pool();
        let _st = st_of(&p);
        seed(&p, &["a1", "a2"]);
        p.observe(
            "a1",
            "default",
            Quota {
                primary_pct: 90.0,
                ..Default::default()
            },
        );
        p.observe(
            "a2",
            "default",
            Quota {
                primary_pct: 5.0,
                ..Default::default()
            },
        );
        let got = p.candidates("gpt-5.6-sol").unwrap();
        assert_eq!(got[0].id, "a2");
    }

    #[test]
    fn model_obs_does_not_mask_account_pool() {
        let p = test_pool();
        let _st = st_of(&p);
        seed(&p, &["a1", "a2"]);
        // a1: fresh model window (0%) but account pool almost exhausted
        p.observe(
            "a1",
            "gpt-6-astra",
            Quota {
                primary_pct: 0.0,
                ..Default::default()
            },
        );
        p.observe(
            "a1",
            "default",
            Quota {
                primary_pct: 93.0,
                ..Default::default()
            },
        );
        p.observe(
            "a2",
            "default",
            Quota {
                primary_pct: 11.0,
                ..Default::default()
            },
        );
        let got = p.select("gpt-6-astra", "").unwrap();
        assert_eq!(
            got[0].id, "a2",
            "0% model window must not hide 93% weekly usage"
        );
    }

    #[test]
    fn pin_holds_while_close_yields_when_far() {
        let p = test_pool();
        let _st = st_of(&p);
        seed(&p, &["a1", "a2", "a3"]);
        p.pin("sess", "a1", "m");
        // close: pinned account within 20pp of best -> stays first
        p.observe(
            "a1",
            "default",
            Quota {
                primary_pct: 30.0,
                ..Default::default()
            },
        );
        p.observe(
            "a2",
            "default",
            Quota {
                primary_pct: 10.0,
                ..Default::default()
            },
        );
        p.observe(
            "a3",
            "default",
            Quota {
                primary_pct: 12.0,
                ..Default::default()
            },
        );
        assert_eq!(p.select("m", "sess").unwrap()[0].id, "a1");
        // far: pinned account lags best by >20pp -> yields
        p.observe(
            "a1",
            "default",
            Quota {
                primary_pct: 93.0,
                ..Default::default()
            },
        );
        assert_eq!(p.select("m", "sess").unwrap()[0].id, "a2");
    }

    #[test]
    fn shadow_ledger_penalizes_unreported_burn_and_reconciles_on_snapshot() {
        // a1 has 10% observed; a2 has 30%. a1 should win — but a1 has just
        // served 50 tokens locally that upstream has not yet reported, and
        // its calibrated rate is 10 tok/pp (astra): pending = 4pp.
        // effective: a1 14 vs a2 30 -> still a1, but above the raw snapshot;
        // a bigger unreported burn flips the order before any snapshot lands.
        let p = test_pool();
        seed(&p, &["a1", "a2"]);
        let now = p.now_secs();
        p.observe(
            "a1",
            "default",
            Quota {
                primary_pct: 10.0,
                secondary_pct: 10.0,
                primary_reset_at: 100,
                secondary_reset_at: 100,
                primary_window_secs: 0,
                secondary_window_secs: 0,
                observed_at: 0,
            },
        );
        p.observe(
            "a2",
            "default",
            Quota {
                primary_pct: 20.0,
                secondary_pct: 20.0,
                ..Default::default()
            },
        );

        // calibrate a1 at 10 tok/pp via a probe sequence: snapshot 0% -> 1% with 10 tokens
        let st = st_of(&p);
        st.add_probe("a1", now - 100, 0.0, 100, "pro");
        st.add_log(&crate::store::LogEntry {
            ts: now - 5,
            account_id: "a1".into(),
            account_email: "a1@x".into(),
            model: "gpt-x".into(),
            status: 200,
            input_tokens: 10,
            cached_tokens: 0,
            output_tokens: 0,
            ..Default::default()
        });
        st.add_probe("a1", now - 4, 1.0, 100, "pro");

        // a1 locally burns 200 more tokens (20pp at its rate) — unreported
        // yet; raw snapshot still favors a1 (10 < 20) but shadow makes it 30
        p.accrue("a1", "gpt-x", 200);
        let c = p.candidates("gpt-5.5").unwrap();
        // shadow pushes a1 to 30 => a2 (20%) first
        assert_eq!(
            c[0].id,
            "a2",
            "unreported burn must demote a1; got {:?}",
            c.iter().map(|x| x.id.clone()).collect::<Vec<_>>()
        );

        // snapshot arrives: reconcile clears the shadow, raw ordering returns
        p.observe(
            "a1",
            "default",
            Quota {
                primary_pct: 12.0,
                secondary_pct: 12.0,
                ..Default::default()
            },
        );
        let c = p.candidates("gpt-5.5").unwrap();
        assert_eq!(
            c[0].id, "a1",
            "12% beats 20% once the snapshot reports reality"
        );
    }
}
