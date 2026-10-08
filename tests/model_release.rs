//! Stable release discovery and catalog availability across failure/recovery.
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use herdex::app::App;
use herdex::config::{Config, ManageCfg, UpstreamCfg};
use herdex::pool::Pool;
use herdex::store::{Account, Store};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

struct Upstream {
    status: StatusCode,
    release: String,
    queries: Vec<(String, String)>,
}

struct Fixture {
    app: Arc<App>,
    store: Store,
    upstream: Arc<Mutex<Upstream>>,
    release_url: String,
    gateway_url: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    root: std::path::PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!("herdex-release-{}", uuid::Uuid::new_v4()));
        let store = Store::open(root.to_str().unwrap()).unwrap();
        store.add_api_key("test-key", "test").unwrap();
        for id in ["a1", "a2", "a3", "a4"] {
            store
                .upsert_account(&Account {
                    id: id.into(),
                    email: format!("{id}@example.test"),
                    plan_type: "pro".into(),
                    account_id: id.into(),
                    access_token: format!("test-token-{id}"),
                    expires_at: herdex::store::now_secs() + 3600,
                    ..Default::default()
                })
                .unwrap();
        }
        let upstream = Arc::new(Mutex::new(Upstream {
            status: StatusCode::SERVICE_UNAVAILABLE,
            release: "unavailable".into(),
            queries: Vec::new(),
        }));
        let upstream_router = Router::new()
            .route(
                "/release",
                get(|State(s): State<Arc<Mutex<Upstream>>>| async move {
                    let s = s.lock().unwrap();
                    (s.status, s.release.clone())
                }),
            )
            .route(
                "/models",
                get(
                    |State(s): State<Arc<Mutex<Upstream>>>,
                     Query(params): Query<HashMap<String, String>>,
                     headers: HeaderMap| async move {
                        let version = params.get("client_version").cloned().unwrap_or_default();
                        assert_eq!(headers.get_all("Version").iter().count(), 1);
                        assert_eq!(headers.get_all("User-Agent").iter().count(), 1);
                        assert_eq!(headers.get("Version").unwrap(), version.as_str());
                        assert_eq!(
                            headers.get("User-Agent").unwrap(),
                            format!("codex_cli_rs/{version}").as_str()
                        );
                        let account = headers
                            .get("Chatgpt-Account-Id")
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_string();
                        s.lock()
                            .unwrap()
                            .queries
                            .push((account.clone(), version.clone()));
                        let mut models = vec![json!({"slug": "gpt-6-sol"})];
                        if account != "a4" {
                            models.push(json!({"slug": "gpt-reserve"}));
                        }
                        if version == "0.159.2" {
                            models.push(json!({"slug": "gpt-6.1-sol"}));
                        }
                        Json(json!({"models": models}))
                    },
                ),
            )
            .route(
                "/responses",
                post(|| async { Json(json!({"id": "response-test", "output": []})) }),
            )
            .with_state(upstream.clone());
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url = format!("http://{}", upstream_listener.local_addr().unwrap());
        let up_task = tokio::spawn(async move {
            axum::serve(upstream_listener, upstream_router)
                .await
                .unwrap();
        });

        let cfg = Config {
            upstream: UpstreamCfg {
                base_url: upstream_url.clone(),
            },
            header_defaults: [
                ("User-Agent".into(), "codex_cli_rs/0.153.4".into()),
                ("Version".into(), "0.156.1".into()),
            ]
            .into_iter()
            .collect(),
            manage: ManageCfg {
                key: "test-manage".into(),
            },
            ..Default::default()
        };
        let app = Arc::new(App {
            cfg,
            store: store.clone(),
            pool: Pool::new(store.clone()),
            http: reqwest::Client::new(),
            model_version: RwLock::new(None),
            usage_root: upstream_url.clone(),
            pending: Default::default(),
            refresh_guards: Default::default(),
            reset_guards: Default::default(),
            last_prune_day: Default::default(),
            learned_strips: Default::default(),
        });
        let gateway_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_url = format!("http://{}", gateway_listener.local_addr().unwrap());
        let gateway_router = herdex::proxy::router().with_state(app.clone());
        let gateway_task = tokio::spawn(async move {
            axum::serve(gateway_listener, gateway_router).await.unwrap();
        });
        Self {
            app,
            store,
            upstream,
            release_url: format!("{upstream_url}/release"),
            gateway_url,
            tasks: vec![up_task, gateway_task],
            root,
        }
    }

    fn set_release(&self, status: StatusCode, body: &str) {
        let mut upstream = self.upstream.lock().unwrap();
        upstream.status = status;
        upstream.release = body.into();
    }

    async fn refresh_catalogs(&self) {
        for a in self.store.list_accounts().unwrap() {
            self.app.refresh_models(&a).await;
        }
    }

    async fn list(&self, path: &str) -> reqwest::Response {
        reqwest::Client::new()
            .get(format!("{}{path}", self.gateway_url))
            .bearer_auth("test-key")
            .send()
            .await
            .unwrap()
    }

    async fn assert_models(&self, expected: &[&str]) {
        let raw: Vec<String> = expected.iter().map(|id| (*id).to_string()).collect();
        let mut v1 = raw.clone();
        for id in expected {
            if id.starts_with("gpt-") && !id.ends_with("-fast") {
                v1.push(format!("{id}-fast"));
            }
        }
        v1.sort();
        v1.dedup();
        for (path, expected) in [("/models", raw.as_slice()), ("/v1/models", v1.as_slice())] {
            let response = self.list(path).await;
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            let body: Value = response.json().await.unwrap();
            let slugs: Vec<_> = body["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].as_str().unwrap().to_string())
                .collect();
            assert_eq!(slugs, expected, "{path}");
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn missing_version_fails_discovery_without_disabling_inference() {
    let f = Fixture::new().await;
    for path in ["/models", "/v1/models"] {
        assert_eq!(f.list(path).await.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            reqwest::Client::new()
                .get(format!("{}{path}", f.gateway_url))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert!(f.app.refresh_model_version(&f.release_url).await.is_err());
    f.refresh_catalogs().await;
    assert!(f.upstream.lock().unwrap().queries.is_empty());
    let response = reqwest::Client::new()
        .post(format!("{}/v1/responses", f.gateway_url))
        .bearer_auth("test-key")
        .json(&json!({"model": "gpt-6-sol", "input": "test", "stream": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn version_changes_failures_and_recovery_recheck_all_accounts() {
    let f = Fixture::new().await;
    f.set_release(StatusCode::OK, r#"{"tag_name":"rust-v0.159.1"}"#);
    assert!(f.app.refresh_model_version(&f.release_url).await.unwrap());
    f.refresh_catalogs().await;
    let models: Value = f.list("/v1/models").await.json().await.unwrap();
    assert_eq!(models["data"].as_array().unwrap().len(), 4); // union plus one Fast alias per GPT model
    assert_eq!(models["data"][0]["id"], "gpt-6-sol");
    assert_eq!(models["data"][1]["id"], "gpt-6-sol-fast");
    assert_eq!(models["data"][2]["id"], "gpt-reserve");
    assert_eq!(models["data"][3]["id"], "gpt-reserve-fast");
    // /models serves the raw union without an extra upstream query; Fast
    // aliases are reserved for the OpenAI-compatible /v1/models endpoint.
    let legacy: Value = f.list("/models").await.json().await.unwrap();
    let legacy_slugs: Vec<_> = legacy["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(legacy_slugs, ["gpt-6-sol", "gpt-reserve"]);
    assert_eq!(f.upstream.lock().unwrap().queries.len(), 4);
    assert!(f
        .upstream
        .lock()
        .unwrap()
        .queries
        .iter()
        .all(|(_, v)| v == "0.159.1"));

    f.set_release(StatusCode::OK, r#"{"tag_name":"rust-v0.159.2"}"#);
    assert!(f.app.refresh_model_version(&f.release_url).await.unwrap());
    f.refresh_catalogs().await;
    let models: Value = f.list("/v1/models").await.json().await.unwrap();
    let slugs: Vec<_> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        slugs,
        [
            "gpt-6-sol",
            "gpt-6-sol-fast",
            "gpt-6.1-sol",
            "gpt-6.1-sol-fast",
            "gpt-reserve",
            "gpt-reserve-fast"
        ]
    );
    assert!(f.upstream.lock().unwrap().queries[4..]
        .iter()
        .all(|(_, v)| v == "0.159.2"));

    f.set_release(StatusCode::OK, r#"{"tag_name":"rust-v0.159.3-beta"}"#);
    assert!(f.app.refresh_model_version(&f.release_url).await.is_err());
    assert!(f.app.model_version().is_none());
    f.refresh_catalogs().await;
    let count = f.upstream.lock().unwrap().queries.len();
    for path in ["/models", "/v1/models"] {
        assert_eq!(f.list(path).await.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
    assert_eq!(f.upstream.lock().unwrap().queries.len(), count);
    assert!(f
        .store
        .account_models("a4")
        .unwrap()
        .contains(&"gpt-6.1-sol".to_string()));

    f.set_release(
        StatusCode::SERVICE_UNAVAILABLE,
        "upstream temporarily unavailable",
    );
    assert!(f.app.refresh_model_version(&f.release_url).await.is_err());
    f.set_release(StatusCode::OK, r#"{"tag_name":"rust-v0.159.2"}"#);
    assert!(f.app.refresh_model_version(&f.release_url).await.unwrap());
    f.refresh_catalogs().await;
    assert_eq!(f.list("/v1/models").await.status(), StatusCode::OK);
}

#[tokio::test]
async fn model_catalog_tracks_account_lifecycle_without_refresh() {
    let f = Fixture::new().await;
    f.set_release(StatusCode::OK, r#"{"tag_name":"rust-v0.159.1"}"#);
    f.app.refresh_model_version(&f.release_url).await.unwrap();
    f.refresh_catalogs().await;
    f.assert_models(&["gpt-6-sol", "gpt-reserve"]).await;

    // Shared models remain advertised while another enabled account has them.
    for id in ["a1", "a2"] {
        f.store.set_account_disabled(id, true).unwrap();
        f.assert_models(&["gpt-6-sol", "gpt-reserve"]).await;
    }
    f.store.set_account_disabled("a3", true).unwrap();
    f.assert_models(&["gpt-6-sol"]).await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/responses", f.gateway_url))
        .bearer_auth("test-key")
        .json(&json!({"model": "gpt-reserve", "input": "test"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    // Re-enabling reuses the retained catalog without another upstream query.
    f.store.set_account_disabled("a1", false).unwrap();
    f.assert_models(&["gpt-6-sol", "gpt-reserve"]).await;
    let candidates = f.app.pool.candidates("gpt-reserve").unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].id, "a1");

    f.store.delete_account("a1").unwrap();
    f.assert_models(&["gpt-6-sol"]).await;
    // Even a late catalog update for a deleted account must stay invisible.
    f.app.pool.set_account_models("a1", &["gpt-orphan".into()]);
    f.assert_models(&["gpt-6-sol"]).await;
    f.store.delete_account("a4").unwrap();
    f.assert_models(&[]).await;
    assert_eq!(f.upstream.lock().unwrap().queries.len(), 4);
}

#[tokio::test]
async fn model_catalog_ignores_missing_and_empty_account_catalogs() {
    let f = Fixture::new().await;
    f.set_release(StatusCode::OK, r#"{"tag_name":"rust-v0.159.1"}"#);
    f.app.refresh_model_version(&f.release_url).await.unwrap();
    f.assert_models(&[]).await;

    f.app
        .pool
        .set_account_models("missing", &["gpt-orphan".into()]);
    f.app.pool.set_account_models("a2", &[]);
    f.assert_models(&[]).await;
    f.app.pool.set_account_models(
        "a1",
        &[
            "gpt-reserve".into(),
            "gpt-6-sol".into(),
            "gpt-reserve".into(),
        ],
    );
    f.assert_models(&["gpt-6-sol", "gpt-reserve"]).await;
    assert!(f.upstream.lock().unwrap().queries.is_empty());
}

#[tokio::test]
async fn model_catalog_account_query_failure_returns_an_error() {
    let f = Fixture::new().await;
    f.set_release(StatusCode::OK, r#"{"tag_name":"rust-v0.159.1"}"#);
    f.app.refresh_model_version(&f.release_url).await.unwrap();
    f.app.pool.set_account_models("a1", &["gpt-6-sol".into()]);
    f.assert_models(&["gpt-6-sol"]).await;

    rusqlite::Connection::open(f.root.join("herdex.db"))
        .unwrap()
        .execute("DROP TABLE accounts", [])
        .unwrap();
    for path in ["/models", "/v1/models"] {
        assert_eq!(
            f.list(path).await.status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
