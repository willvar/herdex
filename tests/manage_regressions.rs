//! Management contracts exercised through the same router as production.
use herdex::app::{build_router, App};
use herdex::config::{Config, ManageCfg};
use herdex::pool::Pool;
use herdex::store::{now_secs, Account, LogEntry, Store};
use herdex::usage::{Limit, Report, Window};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

struct Server {
    app: Arc<App>,
    store: Store,
    url: String,
    task: tokio::task::JoinHandle<()>,
    root: PathBuf,
}

impl Server {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!("herdex-manage-{}", uuid::Uuid::new_v4()));
        let store = Store::open(root.to_str().unwrap()).unwrap();
        let app = Arc::new(App {
            cfg: Config {
                manage: ManageCfg {
                    key: "test-manage-key".into(),
                },
                ..Default::default()
            },
            store: store.clone(),
            pool: Pool::new(store.clone()),
            http: reqwest::Client::new(),
            usage_root: "http://127.0.0.1:1".into(),
            pending: Default::default(),
            refresh_guards: Default::default(),
            last_prune_day: Default::default(),
            learned_strips: Default::default(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/manage/api", listener.local_addr().unwrap());
        let router = build_router(app.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            app,
            store,
            url,
            task,
            root,
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        // Each fixture owns only this newly created, unique directory.
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn history_requires_auth_and_returns_retained_anchors_in_api_shape() {
    let server = Server::new().await;
    let now = now_secs();
    for (id, pct) in [("a", 90.0), ("b", 10.0)] {
        server
            .store
            .upsert_account(&Account {
                id: id.into(),
                email: format!("{id}@example.test"),
                ..Default::default()
            })
            .unwrap();
        server
            .store
            .add_probe(id, now - 32 * 86400, pct, now + 86400, "pro");
    }
    server
        .store
        .add_probe("b", now - 3600, 20.0, now + 86400, "pro");
    server.store.prune_probes(31).unwrap();
    let client = reqwest::Client::new();
    let url = format!("{}/history?days=30", server.url);
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    let response = client
        .get(url)
        .bearer_auth("test-manage-key")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let probes = body["probes"].as_array().unwrap();
    assert_eq!(probes.len(), 3);
    assert_eq!(probes[0]["email"], "a@example.test");
    assert_eq!(probes[0]["used_pct"], 90.0);
    assert_eq!(probes[0]["ts"], now - 32 * 86400);
    assert_eq!(probes[2]["used_pct"], 20.0);
    assert_eq!(body["model_daily"], json!([]));
}

#[tokio::test]
async fn zero_cooldown_survives_save_and_next_state_poll() {
    let server = Server::new().await;
    let client = reqwest::Client::new();
    let settings = json!({"cooldown_seconds": 0, "pin_yield_gap_pp": -1});
    let response = client
        .put(format!("{}/settings", server.url))
        .bearer_auth("test-manage-key")
        .json(&settings)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap(), settings);
    let state: Value = client
        .get(format!("{}/state", server.url))
        .bearer_auth("test-manage-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(state["settings"], settings);
}

#[tokio::test]
async fn calibration_and_history_keep_same_email_accounts_distinct() {
    let server = Server::new().await;
    let now = now_secs();
    let reset = now + 86400;
    for (id, pct, tokens) in [("a", 10.0, 100), ("b", 90.0, 300)] {
        server
            .store
            .upsert_account(&Account {
                id: id.into(),
                email: "shared@example.test".into(),
                plan_type: "pro".into(),
                ..Default::default()
            })
            .unwrap();
        // Use actual observation order, including a completion and probe
        // stamped with the same second, instead of guessing SQL boundaries.
        server.store.add_probe(id, now, pct - 1.0, reset, "pro");
        server.store.add_log(&LogEntry {
            account_id: id.into(),
            account_email: "shared@example.test".into(),
            ts: now,
            input_tokens: tokens,
            status: 200,
            ..Default::default()
        });
        server.app.observe_usage(
            id,
            &Report {
                plan_type: "pro".into(),
                main: Limit {
                    primary: Window {
                        used_pct: Some(pct),
                        reset_at: Some(reset),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
        );
    }

    let client = reqwest::Client::new();
    let calibration: Value = client
        .get(format!("{}/calibration", server.url))
        .bearer_auth("test-manage-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = calibration["accounts"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for (id, pct, tokens) in [("a", 10.0, 100.0), ("b", 90.0, 300.0)] {
        let row = rows.iter().find(|row| row["account_id"] == id).unwrap();
        assert_eq!(row["email"], "shared@example.test");
        assert_eq!(row["used_pct"], pct);
        assert_eq!(row["tokens_per_pct"], tokens);
        assert_eq!(row["samples"], 1);
    }
    assert_eq!(calibration["pool_remaining_tokens"], 12000.0);
    let history: Value = client
        .get(format!("{}/history?days=7", server.url))
        .bearer_auth("test-manage-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let probes = history["probes"].as_array().unwrap();
    for id in ["a", "b"] {
        assert_eq!(
            probes
                .iter()
                .filter(|probe| probe["account_id"] == id)
                .count(),
            2
        );
    }
}

#[tokio::test]
async fn header_probe_without_plan_then_usage_report_preserves_calibration() {
    let server = Server::new().await;
    server
        .store
        .upsert_account(&Account {
            id: "a".into(),
            email: "a@example.test".into(),
            ..Default::default()
        })
        .unwrap();
    let now = now_secs();
    let reset = now + 86400;
    // This is the same add_probe path used for missing plan response headers.
    server.store.add_probe("a", now, 10.0, reset, "");
    server.store.add_log(&LogEntry {
        account_id: "a".into(),
        account_email: "a@example.test".into(),
        ts: now,
        input_tokens: 100,
        status: 200,
        ..Default::default()
    });
    server.app.observe_usage(
        "a",
        &Report {
            plan_type: "pro".into(),
            main: Limit {
                primary: Window {
                    used_pct: Some(11.0),
                    reset_at: Some(reset),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let body: Value = reqwest::Client::new()
        .get(format!("{}/calibration", server.url))
        .bearer_auth("test-manage-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["accounts"][0]["tokens_per_pct"], 100.0);
}
