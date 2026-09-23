use herdex::pool::Pool;
use herdex::store::{now_secs, Account, LogEntry, Settings, Store};
use std::path::PathBuf;

struct TestStore {
    store: Store,
    root: PathBuf,
}

impl TestStore {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("herdex-store-{}", uuid::Uuid::new_v4()));
        let store = Store::open(root.to_str().unwrap()).unwrap();
        Self { store, root }
    }

    fn legacy(with_plan: bool) -> (Self, i64) {
        let root = std::env::temp_dir().join(format!("herdex-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let conn = rusqlite::Connection::open(root.join("herdex.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE request_log (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                account_email TEXT NOT NULL DEFAULT '',
                model TEXT NOT NULL,
                status INTEGER NOT NULL,
                latency_ms INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                cached_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                error TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE usage_probes (
                account_id TEXT NOT NULL,
                ts INTEGER NOT NULL,
                used_pct REAL NOT NULL,
                reset_at INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (account_id, ts)
             );",
        )
        .unwrap();
        if with_plan {
            conn.execute(
                "ALTER TABLE usage_probes ADD COLUMN plan TEXT NOT NULL DEFAULT ''",
                [],
            )
            .unwrap();
        }
        let t = now_secs() - 60;
        for (offset, pct) in [(0, 0.0), (10, 1.0), (20, 2.0)] {
            conn.execute(
                "INSERT INTO usage_probes(account_id,ts,used_pct,reset_at) VALUES ('a',?1,?2,100)",
                rusqlite::params![t + offset, pct],
            )
            .unwrap();
        }
        if with_plan {
            conn.execute("UPDATE usage_probes SET plan='pro' WHERE ts=?1", [t + 10])
                .unwrap();
        }
        for (offset, input, cached, output) in [(5, 80, 60, 20), (15, 160, 120, 40)] {
            conn.execute(
                "INSERT INTO request_log(ts,account_email,model,status,input_tokens,cached_tokens,output_tokens)
                 VALUES (?1,'a@example.test','m',200,?2,?3,?4)",
                rusqlite::params![t + offset, input, cached, output],
            )
            .unwrap();
        }
        drop(conn);
        let store = Store::open(root.to_str().unwrap()).unwrap();
        (Self { store, root }, t)
    }

    fn account(&self, id: &str) {
        self.store
            .upsert_account(&Account {
                id: id.into(),
                email: format!("{id}@example.test"),
                plan_type: "pro".into(),
                ..Default::default()
            })
            .unwrap();
    }

    fn retained_since(&self) -> Option<i64> {
        use rusqlite::OptionalExtension;
        rusqlite::Connection::open(self.root.join("herdex.db"))
            .unwrap()
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM settings WHERE key='request_log_retained_since'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
    }
}

impl Drop for TestStore {
    fn drop(&mut self) {
        // Every instance owns one uniquely created temporary directory.
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn log(store: &Store, ts: i64, input: i64, cached: i64, output: i64) {
    store.add_log(&LogEntry {
        ts,
        account_id: "a".into(),
        account_email: "a@example.test".into(),
        model: "m".into(),
        status: 200,
        input_tokens: input,
        cached_tokens: cached,
        output_tokens: output,
        ..Default::default()
    });
}

#[test]
fn same_second_completed_log_is_counted_before_the_observation() {
    let db = TestStore::new();
    db.account("a");
    let t = now_secs() - 60;
    db.store.add_probe("a", t, 0.0, 100, "pro");
    log(&db.store, t + 10, 80, 60, 20);
    db.store.add_probe("a", t + 10, 1.0, 100, "pro");
    db.store.add_probe("a", t + 20, 1.0, 100, "pro");
    let cal = db.store.calibration("a").unwrap().unwrap();
    assert_eq!((cal.tokens_per_pct, cal.samples), (100.0, 1));
}

#[test]
fn empty_plan_observations_do_not_reset_known_plan_calibration() {
    let db = TestStore::new();
    db.account("a");
    let t = now_secs() - 60;
    db.store.add_probe("a", t, 0.0, 100, "");
    log(&db.store, t + 5, 80, 60, 20);
    db.store.add_probe("a", t + 10, 1.0, 100, "pro");
    log(&db.store, t + 15, 160, 120, 40);
    db.store.add_probe("a", t + 20, 2.0, 100, "");
    let cal = db.store.calibration("a").unwrap().unwrap();
    assert_eq!((cal.tokens_per_pct, cal.samples), (150.0, 2));
    assert!(db
        .store
        .probe_history(1)
        .unwrap()
        .iter()
        .all(|p| p.plan == "pro"));
}

#[test]
fn same_second_probes_preserve_order_and_do_not_claim_later_logs() {
    let db = TestStore::new();
    db.account("a");
    let t = now_secs() - 60;
    db.store.add_probe("a", t, 0.0, 100, "pro");
    log(&db.store, t, 80, 60, 20);
    db.store.add_probe("a", t, 1.0, 100, "pro");
    log(&db.store, t, 240, 180, 60);
    let before = db.store.calibration("a").unwrap().unwrap();
    assert_eq!((before.tokens_per_pct, before.samples), (100.0, 1));

    db.store.add_probe("a", t, 2.0, 100, "pro");
    let after = db.store.calibration("a").unwrap().unwrap();
    assert_eq!((after.tokens_per_pct, after.samples), (200.0, 2));
    let history = db.store.probe_history(1).unwrap();
    assert_eq!(
        history.iter().map(|p| p.used_pct).collect::<Vec<_>>(),
        vec![0.0, 1.0, 2.0]
    );
    assert!(history.iter().all(|p| p.ts == t));
}

#[test]
fn unchanged_quota_keeps_new_logs_until_a_later_quota_increase() {
    let db = TestStore::new();
    db.account("a");
    let t = now_secs() - 60;
    db.store.add_probe("a", t, 0.0, 100, "pro");
    log(&db.store, t, 80, 60, 20);
    db.store.add_probe("a", t, 0.0, 100, "pro");
    db.store.add_probe("a", t, 0.0, 100, "pro");
    assert_eq!(db.store.probe_history(1).unwrap().len(), 2);
    assert!(db.store.calibration("a").unwrap().is_none());
    db.store.add_probe("a", t, 1.0, 100, "pro");
    let cal = db.store.calibration("a").unwrap().unwrap();
    assert_eq!((cal.tokens_per_pct, cal.samples), (100.0, 1));
}

#[test]
fn unchanged_quota_only_records_logs_owned_by_the_account() {
    let db = TestStore::new();
    db.account("a");
    db.account("b");
    let t = now_secs() - 60;
    db.store.add_probe("a", t, 0.0, 100, "pro");
    let mut entry = LogEntry {
        ts: t,
        account_id: "b".into(),
        account_email: "a@example.test".into(),
        status: 200,
        input_tokens: 100,
        ..Default::default()
    };
    // An explicit owner wins over a matching email.
    db.store.add_log(&entry);
    db.store.add_probe("a", t, 0.0, 100, "pro");
    assert_eq!(db.store.probe_history(1).unwrap().len(), 1);

    // Untagged logs with a unique nonempty email still advance the probe.
    entry.account_id.clear();
    db.store.add_log(&entry);
    db.store.add_probe("a", t, 0.0, 100, "pro");
    assert_eq!(db.store.probe_history(1).unwrap().len(), 2);
    db.store.add_probe("a", t, 1.0, 100, "pro");
    assert_eq!(
        db.store.calibration("a").unwrap().unwrap().tokens_per_pct,
        100.0
    );

    db.store
        .upsert_account(&Account {
            id: "b".into(),
            email: "a@example.test".into(),
            plan_type: "pro".into(),
            ..Default::default()
        })
        .unwrap();
    db.store.add_log(&entry);
    db.store.add_probe("a", t, 1.0, 100, "pro");
    assert_eq!(db.store.probe_history(1).unwrap().len(), 3);
    entry.account_id = "a".into();
    db.store.add_log(&entry);
    db.store.add_probe("a", t, 1.0, 100, "pro");
    assert_eq!(db.store.probe_history(1).unwrap().len(), 4);
}

#[test]
fn actual_plan_change_discards_its_interval_and_keeps_the_new_known_plan() {
    let db = TestStore::new();
    db.account("a");
    let t = now_secs() - 60;
    db.store.add_probe("a", t, 0.0, 100, "pro");
    log(&db.store, t + 5, 10_000, 0, 0);
    db.store.add_probe("a", t + 10, 1.0, 100, "plus");
    log(&db.store, t + 15, 80, 60, 20);
    // The account record still says pro, but the latest probe knows plus.
    db.store.add_probe("a", t + 20, 2.0, 100, "");
    let cal = db.store.calibration("a").unwrap().unwrap();
    assert_eq!((cal.tokens_per_pct, cal.samples), (100.0, 1));
    assert_eq!(
        db.store.probe_history(1).unwrap().last().unwrap().plan,
        "plus"
    );
}

#[test]
fn same_email_accounts_only_receive_logs_with_their_own_id() {
    let db = TestStore::new();
    for id in ["a", "b"] {
        db.store
            .upsert_account(&Account {
                id: id.into(),
                email: "shared@example.test".into(),
                plan_type: "pro".into(),
                ..Default::default()
            })
            .unwrap();
    }
    let t = now_secs() - 60;
    for id in ["a", "b"] {
        db.store.add_probe(id, t, 0.0, 100, "pro");
    }
    for (id, tokens) in [("a", 100), ("b", 300), ("", 10_000)] {
        db.store.add_log(&LogEntry {
            ts: t,
            account_id: id.into(),
            account_email: "shared@example.test".into(),
            status: 200,
            input_tokens: tokens,
            ..Default::default()
        });
    }
    for (id, expected) in [("a", 100.0), ("b", 300.0)] {
        db.store.add_probe(id, t, 1.0, 100, "pro");
        let cal = db.store.calibration(id).unwrap().unwrap();
        assert_eq!((cal.tokens_per_pct, cal.samples), (expected, 1));
    }
    assert_eq!(
        db.store
            .recent_logs(10)
            .unwrap()
            .iter()
            .map(|l| l.account_id.as_str())
            .collect::<Vec<_>>(),
        vec!["", "b", "a"]
    );
}

#[test]
fn legacy_logs_require_a_nonempty_unique_email() {
    let db = TestStore::new();
    db.account("a");
    db.store
        .upsert_account(&Account {
            id: "empty".into(),
            ..Default::default()
        })
        .unwrap();
    let t = now_secs() - 60;
    for (id, email) in [("a", "a@example.test"), ("empty", "")] {
        db.store.add_probe(id, t, 0.0, 100, "pro");
        db.store.add_log(&LogEntry {
            ts: t,
            account_email: email.into(),
            status: 200,
            input_tokens: 100,
            ..Default::default()
        });
        db.store.add_probe(id, t, 1.0, 100, "pro");
    }
    let cal = db.store.calibration("a").unwrap().unwrap();
    assert_eq!((cal.tokens_per_pct, cal.samples), (100.0, 1));
    assert!(db.store.calibration("empty").unwrap().is_none());
}

#[test]
fn old_database_migration_preserves_history_and_rebases_into_ordered_samples() {
    let (db, t) = TestStore::legacy(true);
    db.account("a");
    let history = db.store.probe_history(1).unwrap();
    assert_eq!(
        history.iter().map(|p| p.plan.as_str()).collect::<Vec<_>>(),
        vec!["", "pro", ""]
    );
    let old = db.store.calibration("a").unwrap().unwrap();
    assert_eq!((old.tokens_per_pct, old.samples), (150.0, 2));
    assert!(db
        .store
        .recent_logs(10)
        .unwrap()
        .iter()
        .all(|l| l.account_id.is_empty()));

    // The migration cannot reconstruct this interval's exact ordering.
    log(&db.store, t + 20, 10_000, 0, 0);
    db.store.add_probe("a", t + 20, 3.0, 100, "");
    log(&db.store, t + 20, 400, 0, 0);
    db.store.add_probe("a", t + 20, 4.0, 100, "");
    let cal = db.store.calibration("a").unwrap().unwrap();
    assert_eq!(cal.samples, 3);
    assert!((cal.tokens_per_pct - 700.0 / 3.0).abs() < 1e-9);
    let conn = rusqlite::Connection::open(db.root.join("herdex.db")).unwrap();
    let watermarks: Vec<Option<i64>> = conn
        .prepare("SELECT log_seq FROM usage_probes ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(watermarks, vec![None, None, None, Some(3), Some(4)]);

    // Reopening must be idempotent, preserve same-second rows, and not tag legacy data.
    for _ in 0..2 {
        let reopened = Store::open(db.root.to_str().unwrap()).unwrap();
        assert_eq!(reopened.probe_history(1).unwrap().len(), 5);
        let replay = reopened.calibration("a").unwrap().unwrap();
        assert_eq!(
            (replay.tokens_per_pct, replay.samples),
            (cal.tokens_per_pct, 3)
        );
    }
}

#[test]
fn migration_also_accepts_probe_history_without_a_plan_column() {
    let (db, _) = TestStore::legacy(false);
    db.account("a");
    assert!(db
        .store
        .probe_history(1)
        .unwrap()
        .iter()
        .all(|p| p.plan.is_empty()));
    let cal = db.store.calibration("a").unwrap().unwrap();
    assert_eq!((cal.tokens_per_pct, cal.samples), (150.0, 2));
}

#[test]
fn legacy_reset_discards_logs_even_when_completion_order_differs_from_timestamp_order() {
    let (db, t) = TestStore::legacy(false);
    db.account("a");
    let conn = rusqlite::Connection::open(db.root.join("herdex.db")).unwrap();
    conn.execute(
        "UPDATE usage_probes SET reset_at=200 WHERE ts>=?1",
        [t + 10],
    )
    .unwrap();
    // This late insertion belongs to the discarded pre-reset interval.
    log(&db.store, t + 6, 10_000, 0, 0);
    let cal = db.store.calibration("a").unwrap().unwrap();
    assert_eq!((cal.tokens_per_pct, cal.samples), (200.0, 1));
}

#[test]
fn log_watermark_does_not_rewind_after_all_request_logs_are_pruned() {
    let db = TestStore::new();
    db.account("a");
    let t = now_secs() - 60;
    log(&db.store, t - 2 * 86400, 10_000, 0, 0);
    db.store.add_probe("a", t, 0.0, 100, "pro");
    assert_eq!(db.store.prune_logs(1).unwrap(), 1);
    db.store.add_probe("a", t, 0.0, 100, "pro");
    assert_eq!(
        db.store.probe_history(1).unwrap().len(),
        1,
        "cleanup must not create a smaller watermark"
    );
    log(&db.store, t, 100, 0, 0);
    db.store.add_probe("a", t, 1.0, 100, "pro");
    let cal = db.store.calibration("a").unwrap().unwrap();
    assert_eq!((cal.tokens_per_pct, cal.samples), (100.0, 1));
}

#[test]
fn pruning_same_second_anchors_keeps_only_the_last_observation() {
    let db = TestStore::new();
    db.account("a");
    let t = now_secs() - 2 * 86400;
    for pct in [0.0, 1.0, 2.0] {
        db.store.add_probe("a", t, pct, 100, "pro");
    }
    assert_eq!(db.store.prune_probes(1).unwrap(), 2);
    let history = db.store.probe_history(1).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!((history[0].ts, history[0].used_pct), (t, 2.0));
}

#[test]
fn history_includes_each_accounts_last_anchor_and_preserves_resets() {
    let db = TestStore::new();
    for id in ["a", "idle", "new", "deleted"] {
        db.account(id);
    }
    let cutoff = now_secs() - 7 * 86400;
    db.store.add_probe("a", cutoff - 7200, 80.0, 100, "pro");
    db.store.add_probe("a", cutoff - 3600, 90.0, 100, "pro");
    db.store.add_probe("a", cutoff + 3600, 0.0, 200, "pro");
    db.store.add_probe("idle", cutoff - 7200, 20.0, 100, "pro");
    db.store.add_probe("idle", cutoff - 1800, 30.0, 100, "pro");
    db.store.add_probe("new", cutoff + 1800, 5.0, 200, "pro");
    db.store
        .add_probe("deleted", cutoff - 1800, 10.0, 100, "pro");
    db.store
        .add_probe("deleted", cutoff + 1800, 20.0, 100, "pro");
    db.store.delete_account("deleted").unwrap();

    let rows = db.store.probe_history(7).unwrap();
    assert_eq!(rows.len(), 4);
    assert!(rows.windows(2).all(|pair| pair[0].ts <= pair[1].ts));
    assert!(rows
        .iter()
        .all(|p| !p.email.is_empty() && p.account_id != "deleted"));
    let a: Vec<_> = rows.iter().filter(|p| p.account_id == "a").collect();
    assert_eq!(
        (a[0].ts, a[0].used_pct, a[0].reset_at),
        (cutoff - 3600, 90.0, 100)
    );
    assert_eq!(
        (a[1].ts, a[1].used_pct, a[1].reset_at),
        (cutoff + 3600, 0.0, 200)
    );
    let idle = rows.iter().find(|p| p.account_id == "idle").unwrap();
    assert_eq!((idle.ts, idle.used_pct), (cutoff - 1800, 30.0));
}

#[test]
fn pruning_keeps_one_anchor_for_existing_accounts_even_without_new_changes() {
    let db = TestStore::new();
    for id in ["a", "idle", "deleted"] {
        db.account(id);
    }
    let cutoff = now_secs() - 31 * 86400;
    for id in ["a", "idle", "deleted"] {
        db.store.add_probe(id, cutoff - 7200, 10.0, 100, "pro");
        db.store.add_probe(id, cutoff - 3600, 20.0, 100, "pro");
    }
    db.store.add_probe("a", now_secs() - 3600, 30.0, 100, "pro");
    db.store.delete_account("deleted").unwrap();

    assert_eq!(db.store.prune_probes(0).unwrap(), 0);
    assert_eq!(db.store.prune_probes(31).unwrap(), 4);
    assert_eq!(db.store.prune_probes(31).unwrap(), 0);
    let rows = db.store.probe_history(30).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows.iter().filter(|p| p.ts == cutoff - 3600).count(), 2);
    assert!(rows.iter().any(|p| p.account_id == "idle"));
    assert!(rows.iter().all(|p| p.account_id != "deleted"));
}

#[test]
fn probe_deduplication_preserves_reset_and_plan_changes() {
    let db = TestStore::new();
    db.account("a");
    let t = now_secs() - 60;
    db.store.add_probe("a", t, 0.0, 100, "pro");
    db.store.add_probe("a", t + 1, 0.0, 100, "pro");
    db.store.add_probe("a", t + 2, 0.0, 200, "pro");
    db.store.add_probe("a", t + 3, 0.0, 200, "plus");
    let rows = db.store.probe_history(1).unwrap();
    assert_eq!(
        rows.iter().map(|p| p.ts).collect::<Vec<_>>(),
        vec![t, t + 2, t + 3]
    );
}

#[test]
fn calibration_counts_input_once_and_shared_endpoints_once() {
    let db = TestStore::new();
    db.account("a");
    let t = now_secs() - 60;
    db.store.add_probe("a", t, 10.0, 100, "pro");
    log(&db.store, t, 80, 60, 20);
    db.store.add_probe("a", t + 10, 11.0, 100, "pro");
    log(&db.store, t + 10, 160, 120, 40);
    db.store.add_probe("a", t + 20, 12.0, 100, "pro");
    log(&db.store, t + 20, 700, 600, 0);
    let cal = db.store.calibration("a").unwrap().unwrap();
    assert_eq!(cal.samples, 2);
    assert_eq!(cal.tokens_per_pct, 150.0);
}

#[test]
fn calibration_rebases_on_a_reset_even_when_the_percentage_is_unchanged() {
    let db = TestStore::new();
    db.account("a");
    let t = now_secs() - 60;
    db.store.add_probe("a", t, 0.0, 100, "pro");
    log(&db.store, t + 5, 10_000, 0, 0);
    db.store.add_probe("a", t + 10, 0.0, 200, "pro");
    log(&db.store, t + 15, 80, 60, 20);
    db.store.add_probe("a", t + 20, 1.0, 200, "pro");
    let cal = db.store.calibration("a").unwrap().unwrap();
    assert_eq!(cal.samples, 1);
    assert_eq!(cal.tokens_per_pct, 100.0);
}

#[test]
fn retained_anchor_does_not_calibrate_across_deleted_request_logs() {
    let db = TestStore::new();
    db.account("a");
    let cutoff = now_secs() - 86400;
    db.store.add_probe("a", cutoff - 7200, 0.0, 100, "pro");
    db.store.add_probe("a", cutoff - 3600, 10.0, 100, "pro");
    log(&db.store, cutoff - 1800, 900, 0, 0);
    log(&db.store, cutoff + 1800, 100, 0, 0);
    db.store.add_probe("a", cutoff + 3600, 20.0, 100, "pro");
    log(&db.store, cutoff + 5400, 200, 0, 0);
    db.store.add_probe("a", cutoff + 7200, 21.0, 100, "pro");
    assert_eq!(db.store.prune_logs(1).unwrap(), 1);
    assert_eq!(db.store.prune_probes(1).unwrap(), 1);
    let history = db.store.probe_history(1).unwrap();
    assert_eq!(
        history[0].ts,
        cutoff - 3600,
        "chart retains its initial value"
    );
    let cal = db.store.calibration("a").unwrap().unwrap();
    assert_eq!(
        cal.samples, 1,
        "incomplete anchor interval must be excluded"
    );
    assert_eq!(cal.tokens_per_pct, 200.0);

    let reopened = Store::open(db.root.to_str().unwrap()).unwrap();
    assert_eq!(
        reopened.calibration("a").unwrap().unwrap().tokens_per_pct,
        200.0
    );
}

#[test]
fn chart_anchor_can_calibrate_when_its_request_logs_are_complete() {
    let db = TestStore::new();
    db.account("a");
    let cutoff = now_secs() - 86400;
    db.store.add_probe("a", cutoff - 7200, 9.0, 100, "pro");
    db.store.add_probe("a", cutoff - 3600, 10.0, 100, "pro");
    log(&db.store, cutoff - 1800, 80, 60, 20);
    db.store.add_probe("a", cutoff + 3600, 11.0, 100, "pro");
    assert_eq!(db.store.prune_probes(1).unwrap(), 1);
    assert_eq!(
        db.store.calibration("a").unwrap().unwrap().tokens_per_pct,
        100.0
    );
}

#[test]
fn log_cleanup_boundary_only_advances_when_logs_are_deleted() {
    let db = TestStore::new();
    let now = now_secs();
    log(&db.store, now - 10 * 86400, 10, 0, 0);
    assert_eq!(db.store.prune_logs(0).unwrap(), 0);
    assert_eq!(db.retained_since(), None);
    assert_eq!(db.store.prune_logs(5).unwrap(), 1);
    let first = db.retained_since().unwrap();
    assert_eq!(db.store.prune_logs(0).unwrap(), 0);
    assert_eq!(db.store.prune_logs(5).unwrap(), 0);
    assert_eq!(db.store.prune_logs(1).unwrap(), 0);
    assert_eq!(db.retained_since(), Some(first));

    // An imported old log can be deleted under a longer retention setting;
    // it must not make already lost, more recent logs appear complete again.
    log(&db.store, now - 20 * 86400, 10, 0, 0);
    assert_eq!(db.store.prune_logs(15).unwrap(), 1);
    assert_eq!(db.retained_since(), Some(first));
    log(&db.store, now - 2 * 86400, 10, 0, 0);
    assert_eq!(db.store.prune_logs(1).unwrap(), 1);
    assert!(db.retained_since().unwrap() > first);
    assert_eq!(db.store.get_settings().unwrap().cooldown_seconds, 300);
}

#[test]
fn log_cleanup_rolls_back_if_its_retention_boundary_cannot_be_recorded() {
    let db = TestStore::new();
    log(&db.store, now_secs() - 10 * 86400, 100, 0, 0);
    rusqlite::Connection::open(db.root.join("herdex.db"))
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_cleanup_boundary BEFORE INSERT ON settings
             WHEN NEW.key='request_log_retained_since'
             BEGIN SELECT RAISE(ABORT, 'injected boundary write failure'); END;",
        )
        .unwrap();
    assert!(db.store.prune_logs(1).is_err());
    assert_eq!(db.store.recent_logs(10).unwrap().len(), 1);
    assert_eq!(db.retained_since(), None);
}

#[test]
fn explicit_zero_cooldown_survives_roundtrip_and_does_not_exclude_accounts() {
    let db = TestStore::new();
    db.account("a");
    db.account("b");
    assert_eq!(db.store.get_settings().unwrap().cooldown_seconds, 300);
    db.store
        .put_settings(&Settings {
            cooldown_seconds: 0,
            ..Settings::default()
        })
        .unwrap();
    assert_eq!(db.store.get_settings().unwrap().cooldown_seconds, 0);
    let pool = Pool::with_now(db.store.clone(), Box::new(|| 1000));
    pool.mark_failure("a", "m");
    assert_eq!(pool.candidates("m").unwrap().len(), 2);
    assert_eq!(
        serde_json::from_str::<Settings>("{}")
            .unwrap()
            .cooldown_seconds,
        300
    );
}
