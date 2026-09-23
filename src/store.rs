//! Single SQLite database. Migrations preserve existing account, request,
//! and probe history while newer observations carry explicit ordering.

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Empirical capacity calibration for one account: blended tokens-per-1pp
/// (the pool-aggregate weight) from boundary-crossing accounting over the
/// probe sequence. Pure replay — stateless and deterministic. Reset-type
/// events (window rollover, banked-credit refunds, plan changes) invalidate
/// their interval; intervals with no attributed tokens are discarded
/// (external burn / metering drift is not a tokens/pp datapoint).
#[derive(Clone, Debug, Default, Serialize)]
pub struct Calibration {
    pub tokens_per_pct: f64,
    pub samples: i64,
    /// Per-model tokens-per-pp from near-pure intervals. Model mix shifts
    /// (a newer model draining the window at a different rate) make the
    /// long-run blended value stale; per-model rates let consumers project
    /// cost under the CURRENT mix.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub per_model: BTreeMap<String, f64>,
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Clone, Default)]
pub struct Account {
    pub id: String,
    pub email: String,
    pub plan_type: String,
    pub account_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub disabled: bool,
    pub last_error: String,
    pub last_error_at: i64,
}

#[derive(Clone, Default, Serialize)]
pub struct ApiKey {
    pub key: String,
    pub comment: String,
    pub created_at: i64,
    pub disabled: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_cooldown")]
    pub cooldown_seconds: i64,
    /// Migration policy for pinned sessions: -1 = sticky-first (pin rides in
    /// front, migrate only on failure/429); 0 = water-filling chase (always
    /// ride the least-used account, approaches even distribution); >0 =
    /// yield when the pin lags the best by more than this many pp
    #[serde(default = "default_yield_gap")]
    pub pin_yield_gap_pp: i64,
}

fn default_cooldown() -> i64 {
    300
}
fn default_yield_gap() -> i64 {
    20
}
impl Default for Settings {
    fn default() -> Self {
        Settings {
            cooldown_seconds: default_cooldown(),
            pin_yield_gap_pp: default_yield_gap(),
        }
    }
}

#[derive(Clone, Default, Serialize)]
pub struct LogEntry {
    pub ts: i64,
    /// Local account identity; email remains a display label. Empty only for
    /// older log rows whose exact account cannot be recovered unambiguously.
    pub account_id: String,
    pub account_email: String,
    pub api_key: String,
    pub model: String,
    pub status: i64,
    pub latency_ms: i64,
    pub input_tokens: i64,
    pub cached_tokens: i64,
    pub output_tokens: i64,
    pub error: String,
}

#[derive(Clone, Copy)]
pub enum UsageDim {
    Account,
    Key,
    Model,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UsageRow {
    pub label: String,
    pub requests: i64,
    pub input: i64,
    pub cached: i64,
    pub output: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelDayRow {
    pub day: String,
    pub model: String,
    pub input: i64,
    pub cached: i64,
    pub output: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProbePoint {
    pub account_id: String,
    pub email: String,
    pub ts: i64,
    pub used_pct: f64,
    pub reset_at: i64,
    pub plan: String,
}

struct CalibrationProbe {
    ts: i64,
    used_pct: f64,
    reset_at: i64,
    plan: String,
    log_seq: Option<i64>,
}

/// Model catalog served to clients; single source of truth for the proxy
/// /v1/models list and the panel's excluded-model checkboxes.
pub const MODEL_CATALOG: [&str; 6] = [
    "gpt-5.5",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-6-astra",
    "gpt-5.3-codex-spark",
];

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
	id           TEXT PRIMARY KEY,
	email        TEXT NOT NULL,
	plan_type    TEXT NOT NULL,
	account_id   TEXT NOT NULL,
	access_token TEXT NOT NULL,
	refresh_token TEXT NOT NULL,
	expires_at   INTEGER NOT NULL,
	disabled     INTEGER NOT NULL DEFAULT 0,
	last_error   TEXT NOT NULL DEFAULT '',
	last_error_at INTEGER NOT NULL DEFAULT 0,
	created_at   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS api_keys (
	key        TEXT PRIMARY KEY,
	comment    TEXT NOT NULL DEFAULT '',
	created_at INTEGER NOT NULL,
	disabled   INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS settings (
	key   TEXT PRIMARY KEY,
	value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS request_log (
	seq    INTEGER PRIMARY KEY AUTOINCREMENT,
	ts     INTEGER NOT NULL,
	account_id TEXT NOT NULL DEFAULT '',
	account_email TEXT NOT NULL DEFAULT '',
	model  TEXT NOT NULL,
	status INTEGER NOT NULL,
	latency_ms INTEGER NOT NULL DEFAULT 0,
	input_tokens  INTEGER NOT NULL DEFAULT 0,
	cached_tokens INTEGER NOT NULL DEFAULT 0,
	output_tokens INTEGER NOT NULL DEFAULT 0,
	error  TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS usage_probes (
	seq        INTEGER PRIMARY KEY AUTOINCREMENT,
	account_id TEXT NOT NULL,
	ts         INTEGER NOT NULL,
	used_pct   REAL NOT NULL,
	reset_at   INTEGER NOT NULL DEFAULT 0,
	plan       TEXT NOT NULL DEFAULT '',
	log_seq    INTEGER
);
CREATE INDEX IF NOT EXISTS idx_request_log_account_ts ON request_log(account_email, ts);
"#;

impl Store {
    pub fn open(state_root: &str) -> Result<Self, String> {
        std::fs::create_dir_all(state_root).map_err(|e| e.to_string())?;
        let path = format!("{state_root}/herdex.db");
        let conn = Connection::open(&path).map_err(|e| e.to_string())?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))
            .map_err(|e| e.to_string())?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
        let store = Store {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), String> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute_batch(SCHEMA)
            .map_err(|e| format!("migrate: {e}"))?;
        // v1: api_keys.label renamed to comment (data-preserving on old DBs)
        let _ = conn.execute("ALTER TABLE api_keys RENAME COLUMN label TO comment", []);
        // v2: token usage stats need the api_key dimension on request_log
        let cols: Vec<String> = {
            let mut stmt = conn
                .prepare("PRAGMA table_info(request_log)")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(1))
                .map_err(|e| e.to_string())?;
            rows.filter_map(|r| r.ok()).collect()
        };
        if !cols.iter().any(|c| c == "api_key") {
            conn.execute(
                "ALTER TABLE request_log ADD COLUMN api_key TEXT NOT NULL DEFAULT ''",
                [],
            )
            .map_err(|e| format!("migrate: {e}"))?;
        }
        if !cols.iter().any(|c| c == "account_id") {
            conn.execute(
                "ALTER TABLE request_log ADD COLUMN account_id TEXT NOT NULL DEFAULT ''",
                [],
            )
            .map_err(|e| format!("migrate: {e}"))?;
        }
        // v3: usage_probes gained a plan column (plan change resets accounting)
        let probe_cols: Vec<String> = {
            let mut stmt = conn
                .prepare("PRAGMA table_info(usage_probes)")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(1))
                .map_err(|e| e.to_string())?;
            rows.filter_map(|r| r.ok()).collect()
        };
        if !probe_cols.iter().any(|c| c == "plan") {
            conn.execute(
                "ALTER TABLE usage_probes ADD COLUMN plan TEXT NOT NULL DEFAULT ''",
                [],
            )
            .map_err(|e| format!("migrate: {e}"))?;
        }
        // Same-second observations need distinct identities. Legacy samples
        // retain NULL watermarks: their actual ordering relative to logs is
        // unknowable and must not be invented during migration.
        if !probe_cols.iter().any(|c| c == "seq") {
            let tx = conn.transaction().map_err(|e| format!("migrate: {e}"))?;
            tx.execute_batch(
                "CREATE TABLE usage_probes_ordered (
                     seq INTEGER PRIMARY KEY AUTOINCREMENT,
                     account_id TEXT NOT NULL,
                     ts INTEGER NOT NULL,
                     used_pct REAL NOT NULL,
                     reset_at INTEGER NOT NULL DEFAULT 0,
                     plan TEXT NOT NULL DEFAULT '',
                     log_seq INTEGER
                 );
                 INSERT INTO usage_probes_ordered(account_id,ts,used_pct,reset_at,plan)
                     SELECT account_id,ts,used_pct,reset_at,plan FROM usage_probes
                     ORDER BY ts,account_id;
                 DROP TABLE usage_probes;
                 ALTER TABLE usage_probes_ordered RENAME TO usage_probes;",
            )
            .map_err(|e| format!("migrate: {e}"))?;
            tx.commit().map_err(|e| format!("migrate: {e}"))?;
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_usage_probes_account_seq ON usage_probes(account_id,seq);
             CREATE INDEX IF NOT EXISTS idx_request_log_account_id_seq ON request_log(account_id,seq);",
        )
        .map_err(|e| format!("migrate: {e}"))?;
        Ok(())
    }

    pub fn upsert_account(&self, a: &Account) -> Result<(), String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            r#"INSERT INTO accounts
                (id,email,plan_type,account_id,access_token,refresh_token,expires_at,disabled,last_error,last_error_at,created_at)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                ON CONFLICT(id) DO UPDATE SET
                email=excluded.email, plan_type=excluded.plan_type, account_id=excluded.account_id,
                access_token=excluded.access_token, refresh_token=excluded.refresh_token,
                expires_at=excluded.expires_at, disabled=excluded.disabled,
                last_error=excluded.last_error, last_error_at=excluded.last_error_at"#,
            rusqlite::params![
                a.id, a.email, a.plan_type, a.account_id, a.access_token, a.refresh_token,
                a.expires_at, a.disabled as i64, a.last_error, a.last_error_at, now_secs()
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn update_tokens(
        &self,
        id: &str,
        access: &str,
        refresh: &str,
        expires_at: i64,
    ) -> Result<(), String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let n = conn
            .execute(
                "UPDATE accounts SET access_token=?1, refresh_token=?2, expires_at=?3 WHERE id=?4",
                rusqlite::params![access, refresh, expires_at, id],
            )
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err(format!("account {id}: not found"));
        }
        Ok(())
    }

    pub fn set_account_disabled(&self, id: &str, disabled: bool) -> Result<(), String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let n = conn
            .execute(
                "UPDATE accounts SET disabled=?1 WHERE id=?2",
                rusqlite::params![disabled as i64, id],
            )
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err(format!("account {id}: not found"));
        }
        Ok(())
    }

    pub fn set_account_plan(&self, id: &str, plan: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "UPDATE accounts SET plan_type=?1 WHERE id=?2",
            rusqlite::params![plan, id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn record_account_error(&self, id: &str, msg: &str) {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let _ = conn.execute(
            "UPDATE accounts SET last_error=?1, last_error_at=?2 WHERE id=?3",
            rusqlite::params![msg, now_secs(), id],
        );
    }

    pub fn delete_account(&self, id: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let n = conn
            .execute("DELETE FROM accounts WHERE id=?1", rusqlite::params![id])
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err(format!("account {id}: not found"));
        }
        Ok(())
    }

    fn account_from_row(row: &rusqlite::Row) -> Result<Account, String> {
        Ok(Account {
            id: row.get(0).map_err(|e| e.to_string())?,
            email: row.get(1).map_err(|e| e.to_string())?,
            plan_type: row.get(2).map_err(|e| e.to_string())?,
            account_id: row.get(3).map_err(|e| e.to_string())?,
            access_token: row.get(4).map_err(|e| e.to_string())?,
            refresh_token: row.get(5).map_err(|e| e.to_string())?,
            expires_at: row.get(6).map_err(|e| e.to_string())?,
            disabled: row.get::<_, i64>(7).map_err(|e| e.to_string())? != 0,
            last_error: row.get(8).map_err(|e| e.to_string())?,
            last_error_at: row.get(9).map_err(|e| e.to_string())?,
        })
    }

    const ACCOUNT_COLS: &str =
        "id,email,plan_type,account_id,access_token,refresh_token,expires_at,disabled,last_error,last_error_at";

    pub fn get_account(&self, id: &str) -> Result<Account, String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let sql = format!("SELECT {} FROM accounts WHERE id=?1", Self::ACCOUNT_COLS);
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query(rusqlite::params![id])
            .map_err(|e| e.to_string())?;
        match rows.next().map_err(|e| e.to_string())? {
            Some(row) => Self::account_from_row(row),
            None => Err(format!("account {id}: not found")),
        }
    }

    pub fn list_accounts(&self) -> Result<Vec<Account>, String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let sql = format!(
            "SELECT {} FROM accounts ORDER BY created_at",
            Self::ACCOUNT_COLS
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            out.push(Self::account_from_row(row)?);
        }
        Ok(out)
    }

    pub fn add_api_key(&self, key: &str, comment: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO api_keys(key,comment,created_at) VALUES (?1,?2,?3)",
            rusqlite::params![key, comment, now_secs()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn delete_api_key(&self, key: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let n = conn
            .execute("DELETE FROM api_keys WHERE key=?1", rusqlite::params![key])
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err(format!("api key {key}: not found"));
        }
        Ok(())
    }

    pub fn set_api_key_disabled(&self, key: &str, disabled: bool) -> Result<(), String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let n = conn
            .execute(
                "UPDATE api_keys SET disabled=?1 WHERE key=?2",
                rusqlite::params![disabled as i64, key],
            )
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err(format!("api key {key}: not found"));
        }
        Ok(())
    }

    pub fn set_api_key_comment(&self, key: &str, comment: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let n = conn
            .execute(
                "UPDATE api_keys SET comment=?1 WHERE key=?2",
                rusqlite::params![comment, key],
            )
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err(format!("api key {key}: not found"));
        }
        Ok(())
    }

    pub fn update_api_key(&self, old_key: &str, new_key: &str) -> Result<(), String> {
        if old_key == new_key {
            return Ok(());
        }
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let n = conn
            .execute(
                "UPDATE api_keys SET key=?1 WHERE key=?2",
                rusqlite::params![new_key, old_key],
            )
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err(format!("api key {old_key}: not found"));
        }
        Ok(())
    }

    pub fn list_api_keys(&self) -> Result<Vec<ApiKey>, String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn
            .prepare("SELECT key,comment,created_at,disabled FROM api_keys ORDER BY created_at")
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            out.push(ApiKey {
                key: row.get(0).map_err(|e| e.to_string())?,
                comment: row.get(1).map_err(|e| e.to_string())?,
                created_at: row.get(2).map_err(|e| e.to_string())?,
                disabled: row.get::<_, i64>(3).map_err(|e| e.to_string())? != 0,
            });
        }
        Ok(out)
    }

    pub fn api_key_valid(&self, key: &str) -> Result<bool, String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn
            .prepare("SELECT COUNT(1) FROM api_keys WHERE key=?1 AND disabled=0")
            .map_err(|e| e.to_string())?;
        let n: i64 = stmt
            .query_row(rusqlite::params![key], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        Ok(n > 0)
    }

    pub fn get_settings(&self) -> Result<Settings, String> {
        // NOTE: the connection lock must be released before any nested store
        // call (put_settings re-locks; a scoped lock avoids self-deadlock —
        // the Go version never had this problem because database/sql is a
        // connection pool, not a single guarded connection).
        let raw = {
            let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
            conn.query_row("SELECT value FROM settings WHERE key='pool'", [], |r| {
                r.get::<_, String>(0)
            })
        };
        match raw {
            Ok(raw) => serde_json::from_str(&raw).map_err(|e| format!("settings: {e}")),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                let def = Settings::default();
                self.put_settings(&def)?;
                Ok(def)
            }
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn put_settings(&self, st: &Settings) -> Result<(), String> {
        let raw = serde_json::to_string(st).map_err(|e| e.to_string())?;
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO settings(key,value) VALUES ('pool',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            rusqlite::params![raw],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn add_log(&self, e: &LogEntry) {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let _ = conn.execute(
            "INSERT INTO request_log(ts,account_email,api_key,model,status,latency_ms,input_tokens,cached_tokens,output_tokens,error,account_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            rusqlite::params![
                e.ts, e.account_email, e.api_key, e.model, e.status, e.latency_ms,
                e.input_tokens, e.cached_tokens, e.output_tokens, e.error, e.account_id
            ],
        );
    }

    /// Token usage aggregates for the panel. `col` must be a whitelisted
    /// request_log column; buckets are VM-local days.
    pub fn usage_daily(&self, days: i64) -> Result<Vec<UsageRow>, String> {
        let since = now_secs() - days * 86400;
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn
            .prepare(
                r#"SELECT date(ts,'unixepoch','localtime') d, COUNT(*),
                          SUM(input_tokens), SUM(cached_tokens), SUM(output_tokens)
                   FROM request_log WHERE ts >= ?1 GROUP BY d ORDER BY d"#,
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![since], |r| {
                Ok(UsageRow {
                    label: r.get::<_, String>(0).unwrap_or_default(),
                    requests: r.get::<_, i64>(1).unwrap_or(0),
                    input: r.get::<_, i64>(2).unwrap_or(0),
                    cached: r.get::<_, i64>(3).unwrap_or(0),
                    output: r.get::<_, i64>(4).unwrap_or(0),
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn usage_by(&self, col: UsageDim, days: i64) -> Result<Vec<UsageRow>, String> {
        let column = match col {
            UsageDim::Account => "account_email",
            UsageDim::Key => "api_key",
            UsageDim::Model => "model",
        };
        let since = now_secs() - days * 86400;
        let sql = format!(
            r#"SELECT {column}, COUNT(*), SUM(input_tokens), SUM(cached_tokens), SUM(output_tokens)
               FROM request_log WHERE ts >= ?1 AND {column} != '' GROUP BY {column}
               ORDER BY SUM(input_tokens) + SUM(output_tokens) DESC"#
        );
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![since], |r| {
                Ok(UsageRow {
                    label: r.get::<_, String>(0).unwrap_or_default(),
                    requests: r.get::<_, i64>(1).unwrap_or(0),
                    input: r.get::<_, i64>(2).unwrap_or(0),
                    cached: r.get::<_, i64>(3).unwrap_or(0),
                    output: r.get::<_, i64>(4).unwrap_or(0),
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    /// Per-day, per-model token sums for the panel's model-burn comparison.
    pub fn usage_daily_by_model(&self, days: i64) -> Result<Vec<ModelDayRow>, String> {
        let since = now_secs() - days * 86400;
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn
            .prepare(
                r#"SELECT date(ts,'unixepoch','localtime') d, model,
                          SUM(input_tokens), SUM(cached_tokens), SUM(output_tokens)
                   FROM request_log WHERE ts >= ?1 AND model != ''
                   GROUP BY d, model ORDER BY d, model"#,
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![since], |r| {
                Ok(ModelDayRow {
                    day: r.get(0)?,
                    model: r.get(1)?,
                    input: r.get(2)?,
                    cached: r.get(3)?,
                    output: r.get(4)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    /// Probe history for existing accounts, including the last observation
    /// before the requested window. Samples record changes, so this anchor
    /// preserves the initial value even when an account stays unchanged.
    pub fn probe_history(&self, days: i64) -> Result<Vec<ProbePoint>, String> {
        let since = now_secs() - days * 86400;
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn
            .prepare(
                r#"SELECT p.account_id, a.email, p.ts, p.used_pct, p.reset_at, p.plan
                   FROM usage_probes p JOIN accounts a ON a.id = p.account_id
                   WHERE p.ts >= ?1 OR p.seq IN (
                       SELECT MAX(seq) FROM usage_probes
                       WHERE ts < ?1 GROUP BY account_id
                   )
                   ORDER BY p.ts, p.seq"#,
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![since], |r| {
                Ok(ProbePoint {
                    account_id: r.get(0)?,
                    email: r.get(1)?,
                    ts: r.get(2)?,
                    used_pct: r.get(3)?,
                    reset_at: r.get(4)?,
                    plan: r.get(5)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    /// Persists a probe sample (wham probe or response-header observation).
    /// Capture the completed-log watermark under the same lock as insertion.
    /// New account logs matter even when upstream quota has not caught up yet.
    /// Empty plan metadata never erases a previously known plan.
    pub fn add_probe(&self, account_id: &str, ts: i64, used_pct: f64, reset_at: i64, plan: &str) {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let result = (|| -> rusqlite::Result<()> {
            // sqlite_sequence survives retention cleanup, unlike MAX(seq)
            // over request_log when all completed requests have been pruned.
            let log_seq: i64 = conn.query_row(
                "SELECT COALESCE(MAX(seq),0) FROM sqlite_sequence WHERE name='request_log'",
                [],
                |r| r.get(0),
            )?;
            let plan = if plan.is_empty() {
                conn.query_row(
                    "SELECT plan FROM usage_probes WHERE account_id=?1 AND plan!='' ORDER BY seq DESC LIMIT 1",
                    [account_id],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .or(conn.query_row("SELECT plan_type FROM accounts WHERE id=?1", [account_id], |r| r.get(0)).optional()?)
                .unwrap_or_default()
            } else {
                plan.to_owned()
            };
            let last: Option<(f64, String, i64, Option<i64>)> = conn
                .query_row(
                    "SELECT used_pct, plan, reset_at, log_seq FROM usage_probes WHERE account_id=?1 ORDER BY seq DESC LIMIT 1",
                    [account_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            if let Some((p, pl, rr, previous_log_seq)) = last {
                if (p - used_pct).abs() < f64::EPSILON && pl == plan && rr == reset_at {
                    if let Some(previous) = previous_log_seq {
                        if previous == log_seq {
                            return Ok(());
                        }
                        let has_logs: bool = conn.query_row(
                            "SELECT EXISTS(SELECT 1 FROM request_log WHERE seq>?1 AND
                             (account_id=?2 OR (account_id='' AND account_email IN (
                                 SELECT email FROM accounts WHERE id=?2 AND email!=''
                                 AND (SELECT COUNT(*) FROM accounts a WHERE a.email=accounts.email)=1
                             ))))",
                            rusqlite::params![previous, account_id],
                            |r| r.get(0),
                        )?;
                        if !has_logs {
                            return Ok(());
                        }
                    }
                }
            }
            conn.execute(
                "INSERT INTO usage_probes (account_id,ts,used_pct,reset_at,plan,log_seq) VALUES (?1,?2,?3,?4,?5,?6)",
                rusqlite::params![account_id, ts, used_pct, reset_at, plan, log_seq],
            )?;
            Ok(())
        })();
        if let Err(e) = result {
            log::warn!("record probe for {account_id}: {e}");
        }
    }

    /// Keep one pre-cutoff anchor for each existing account, including
    /// accounts with no newer changes. Deleted accounts need no anchor.
    pub fn prune_probes(&self, days: i64) -> Result<usize, String> {
        if days <= 0 {
            return Ok(0);
        }
        let cutoff = now_secs() - days * 86400;
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "DELETE FROM usage_probes WHERE ts < ?1 AND seq NOT IN (
                 SELECT MAX(p.seq) FROM usage_probes p
                 JOIN accounts a ON a.id = p.account_id
                 WHERE p.ts < ?1 GROUP BY p.account_id
             )",
            rusqlite::params![cutoff],
        )
        .map_err(|e| e.to_string())
    }

    /// Empirical capacity calibration for one account, by replaying the probe
    /// sequence against request_log. Pure replay — stateless and
    /// deterministic. Reset-type events (window rollover, banked-credit
    /// refunds, plan changes) invalidate their interval.
    ///
    /// Two layers of answer:
    /// - `tokens_per_pct`: blended tokens per 1pp across all models — the
    ///   account weight for pool-aggregate usage. Same semantics as the
    ///   original boundary-crossing accounting (1pp floor, remainder held).
    pub fn calibration(&self, account_id: &str) -> Result<Option<Calibration>, String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let email: String = match conn.query_row(
            "SELECT email FROM accounts WHERE id=?1",
            rusqlite::params![account_id],
            |r| r.get(0),
        ) {
            Ok(e) => e,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        // A chart anchor may predate retained request logs. Only complete
        // intervals can calibrate capacity; never attribute the remaining
        // fraction of a pruned interval to its full percentage change.
        let logs_since = conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM settings WHERE key='request_log_retained_since'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(|e| e.to_string())?
            .unwrap_or(0);
        let unique_email: bool = conn
            .query_row(
                "SELECT ?1!='' AND COUNT(*)=1 FROM accounts WHERE email=?1",
                [&email],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        let probes: Vec<CalibrationProbe> = {
            let mut stmt = conn
                .prepare("SELECT ts, used_pct, reset_at, plan, log_seq FROM usage_probes WHERE account_id=?1 AND ts>=?2 ORDER BY seq")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(rusqlite::params![account_id, logs_since], |r| {
                    Ok(CalibrationProbe {
                        ts: r.get(0)?,
                        used_pct: r.get(1)?,
                        reset_at: r.get(2)?,
                        plan: r.get(3)?,
                        log_seq: r.get(4)?,
                    })
                })
                .map_err(|e| e.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?
        };
        if probes.len() < 2 {
            return Ok(None);
        }

        // blended boundary-crossing accounting
        let mut sum_tokens: f64 = 0.0;
        let mut sum_pp: f64 = 0.0;
        let mut samples = 0i64;
        let mut accum: f64 = 0.0;
        let mut ref_pct = probes[0].used_pct;
        let mut cur_plan = probes[0].plan.clone();
        let mut cur_reset = probes[0].reset_at;
        // Legacy timestamp replay is best-effort. Never let it claim logs
        // belonging to the new ordered segment; its transition is a re-base.
        let legacy_end = probes.iter().find_map(|p| p.log_seq).unwrap_or(i64::MAX);
        // Fetch each ordering domain once. Legacy samples were migrated in
        // timestamp order; new samples use the global completed-log sequence.
        // IDs own tagged logs even if their email matches another account.
        let legacy_logs: Vec<(i64, String, i64)> = if probes[0].log_seq.is_none() {
            let last = probes
                .iter()
                .take_while(|p| p.log_seq.is_none())
                .last()
                .unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT ts, model, input_tokens+output_tokens FROM request_log
                 WHERE status=200 AND ts>=?1 AND ts<?2 AND seq<=?3 AND
                   (account_id=?4 OR (account_id='' AND ?5 AND account_email=?6))
                 ORDER BY ts",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(
                    rusqlite::params![
                        probes[0].ts,
                        last.ts,
                        legacy_end,
                        account_id,
                        unique_email,
                        email
                    ],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .map_err(|e| e.to_string())?;
            rows.collect::<Result<_, _>>().map_err(|e| e.to_string())?
        } else {
            Vec::new()
        };
        let ordered_logs: Vec<(i64, String, i64)> =
            if let Some(end) = probes.last().and_then(|p| p.log_seq) {
                let mut stmt = conn
                    .prepare(
                        "SELECT seq, model, input_tokens+output_tokens FROM request_log
                 WHERE status=200 AND seq>?1 AND seq<=?2 AND
                   (account_id=?3 OR (account_id='' AND ?4 AND account_email=?5))
                 ORDER BY seq",
                    )
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(
                        rusqlite::params![legacy_end, end, account_id, unique_email, email],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .map_err(|e| e.to_string())?;
                rows.collect::<Result<_, _>>().map_err(|e| e.to_string())?
            } else {
                Vec::new()
            };
        drop(conn);
        let mut legacy_pos = 0;
        let mut ordered_pos = 0;
        // per-model accumulators over near-pure intervals; the blended sums
        // stay untouched so existing weights keep their semantics
        let mut sum_tokens_by_model: BTreeMap<String, f64> = BTreeMap::new();
        let mut sum_pp_by_model: BTreeMap<String, f64> = BTreeMap::new();

        for i in 1..probes.len() {
            let p0 = &probes[i - 1];
            let p1 = &probes[i];
            // Consume even invalidated intervals, so reset/plan transitions
            // cannot leave old tokens for a later interval to pick up.
            let mut tokens = 0i64;
            let mut per_model_tokens: BTreeMap<String, f64> = BTreeMap::new();
            match (p0.log_seq, p1.log_seq) {
                (Some(start), Some(end)) => {
                    while let Some(&(seq, ref model, amount)) = ordered_logs.get(ordered_pos) {
                        if seq > end {
                            break;
                        }
                        ordered_pos += 1;
                        if seq > start {
                            tokens += amount;
                            *per_model_tokens.entry(model.clone()).or_insert(0.0) += amount as f64;
                        }
                    }
                }
                (None, None) => {
                    while let Some(&(ts, ref model, amount)) = legacy_logs.get(legacy_pos) {
                        if ts >= p1.ts {
                            break;
                        }
                        legacy_pos += 1;
                        if ts >= p0.ts {
                            tokens += amount;
                            *per_model_tokens.entry(model.clone()).or_insert(0.0) += amount as f64;
                        }
                    }
                }
                _ => {}
            }
            // Unknown metadata is not evidence of a subscription change.
            let plan_changed = !cur_plan.is_empty() && !p1.plan.is_empty() && p1.plan != cur_plan;
            if !p1.plan.is_empty() {
                cur_plan.clone_from(&p1.plan);
            }
            if plan_changed
                || p1.reset_at != cur_reset
                || p0.log_seq.is_some() != p1.log_seq.is_some()
            {
                cur_reset = p1.reset_at;
                ref_pct = p1.used_pct;
                accum = 0.0;
                continue;
            }
            // Input already contains cached input.
            let total = accum + tokens as f64;
            if p1.used_pct > ref_pct {
                let delta = p1.used_pct - ref_pct;
                // blended: boundary-crossing with 1pp floor. An interval
                // with no locally-attributed tokens is external burn (the
                // account consumed elsewhere) or async metering drift —
                // either way it is NOT a tokens/pp datapoint: discard it
                // together with any held remainder.
                if total <= 0.0 {
                    accum = 0.0;
                } else {
                    let crossed = delta.floor();
                    if crossed >= 1.0 {
                        sum_tokens += total;
                        sum_pp += crossed;
                        samples += 1;
                        // near-pure intervals teach a per-model rate: one
                        // model carrying ≥98% of the interval's tokens owns
                        // the pp movement, so the mixed-interval ambiguity
                        // (which model moved how many pp) never arises
                        let dominant = per_model_tokens.values().copied().fold(0.0, f64::max);
                        if let Some((m, _mtok)) = per_model_tokens
                            .iter()
                            .find(|(_, t)| **t >= dominant * 0.98)
                        {
                            let e = sum_tokens_by_model.entry(m.clone()).or_insert(0.0);
                            *e += total;
                            let p = sum_pp_by_model.entry(m.clone()).or_insert(0.0);
                            *p += crossed;
                        }
                        accum = 0.0; // sub-pp remainder discarded (≤1pp bound)
                    } else {
                        accum = total; // moved <1pp: keep accumulating
                    }
                }
                ref_pct = p1.used_pct;
            } else if p1.used_pct < ref_pct {
                // % drop without crossing up: reset-type event (banked credit,
                // global reset) — refund without pool tokens; discard in-flight
                accum = 0.0;
                ref_pct = p1.used_pct;
            } else {
                accum = total; // quota may lag completed request logs
            }
        }
        if sum_pp <= 0.0 {
            return Ok(None);
        }
        let blended = sum_tokens / sum_pp;
        let per_model = sum_pp_by_model
            .iter()
            .filter_map(|(m, pp)| {
                let t = *sum_tokens_by_model.get(m)?;
                if *pp > 0.0 {
                    Some((m.clone(), t / *pp))
                } else {
                    None
                }
            })
            .collect();
        Ok(Some(Calibration {
            tokens_per_pct: blended,
            samples,
            per_model,
        }))
    }

    /// Deletes request_log rows older than `days` (VM-local). Returns rows removed.
    pub fn prune_logs(&self, days: i64) -> Result<usize, String> {
        if days <= 0 {
            return Ok(0);
        }
        let cutoff = now_secs() - days * 86400;
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        let removed = tx
            .execute(
                "DELETE FROM request_log WHERE ts < ?1",
                rusqlite::params![cutoff],
            )
            .map_err(|e| e.to_string())?;
        if removed > 0 {
            // Retention increases cannot restore deleted logs. Persist the
            // furthest actual cleanup boundary in the same transaction.
            tx.execute(
                "INSERT INTO settings(key,value) VALUES ('request_log_retained_since',?1)
                 ON CONFLICT(key) DO UPDATE SET value=MAX(CAST(value AS INTEGER),CAST(excluded.value AS INTEGER))",
                rusqlite::params![cutoff],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(removed)
    }

    pub fn recent_logs(&self, limit: i64) -> Result<Vec<LogEntry>, String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn
            .prepare(
                "SELECT ts,account_email,api_key,model,status,latency_ms,input_tokens,cached_tokens,output_tokens,error,account_id
                 FROM request_log ORDER BY seq DESC LIMIT ?1",
            )
            .map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query(rusqlite::params![limit])
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            out.push(LogEntry {
                ts: row.get(0).map_err(|e| e.to_string())?,
                account_email: row.get(1).map_err(|e| e.to_string())?,
                api_key: row.get(2).map_err(|e| e.to_string())?,
                model: row.get(3).map_err(|e| e.to_string())?,
                status: row.get(4).map_err(|e| e.to_string())?,
                latency_ms: row.get(5).map_err(|e| e.to_string())?,
                input_tokens: row.get(6).map_err(|e| e.to_string())?,
                cached_tokens: row.get(7).map_err(|e| e.to_string())?,
                output_tokens: row.get(8).map_err(|e| e.to_string())?,
                error: row.get(9).map_err(|e| e.to_string())?,
                account_id: row.get(10).map_err(|e| e.to_string())?,
            });
        }
        Ok(out)
    }
}
