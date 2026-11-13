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
    catalog_overrides: HashMap<String, Value>,
    usage_plan: String,
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

fn catalog_model(slug: &str) -> Value {
    json!({
        "slug": slug,
        "display_name": slug,
        "description": format!("metadata for {slug}"),
        "use_responses_lite": slug != "gpt-5.5",
        "web_search_tool_type": "text_and_image",
        "catalog_marker": format!("marker-{slug}"),
        "service_tiers": [{"id": "priority", "name": "Fast", "description": "Synthetic Fast capability."}],
    })
}

impl Fixture {
    async fn new() -> Self {
        Self::with_models(Vec::new()).await
    }

    async fn with_models(models: Vec<String>) -> Self {
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
            catalog_overrides: HashMap::new(),
            usage_plan: "pro".into(),
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
                        if let Some(payload) = s.lock().unwrap().catalog_overrides.get(&account).cloned() {
                            return Json(payload);
                        }
                        let mut models = vec![catalog_model("gpt-6-sol")];
                        if account != "a4" {
                            models.push(catalog_model("gpt-reserve"));
                        }
                        if version == "0.159.2" {
                            models.push(catalog_model("gpt-6.1-sol"));
                        }
                        Json(json!({"models": models}))
                    },
                ),
            )
            .route(
                "/backend-api/wham/usage",
                get(|State(s): State<Arc<Mutex<Upstream>>>| async move {
                    Json(json!({"plan_type":s.lock().unwrap().usage_plan, "rate_limit":{
                        "primary_window":{"used_percent":12,"limit_window_seconds":604800,"reset_at":1893456000}
                    }}))
                }),
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
            models,
            upstream: UpstreamCfg {
                base_url: upstream_url.clone(),
                standalone_search_timeout_seconds: 45,
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
            routing: Default::default(),
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
        self.list_with_key(path, "test-key").await
    }

    async fn list_with_key(&self, path: &str, key: &str) -> reqwest::Response {
        reqwest::Client::new()
            .get(format!("{}{path}", self.gateway_url))
            .bearer_auth(key)
            .send()
            .await
            .unwrap()
    }

    async fn assert_v1_models(&self, expected: &[&str]) {
        let raw: Vec<String> = expected.iter().map(|id| (*id).to_string()).collect();
        let response = self.list("/v1/models").await;
        assert_eq!(response.status(), StatusCode::OK, "/v1/models");
        let body: Value = response.json().await.unwrap();
        let slugs: Vec<_> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(slugs, raw);
    }

    async fn assert_codex_models(&self, expected: &[&str]) {
        let raw: Vec<String> = expected.iter().map(|id| (*id).to_string()).collect();
        let response = self.list("/models").await;
        assert_eq!(response.status(), StatusCode::OK, "/models");
        let body: Value = response.json().await.unwrap();
        let slugs: Vec<_> = body["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["slug"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(slugs, raw);
        for model in body["models"].as_array().unwrap() {
            assert!(model["catalog_marker"].as_str().is_some());
        }
    }

    async fn assert_models(&self, expected: &[&str]) {
        self.assert_v1_models(expected).await;
        self.assert_codex_models(expected).await;
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
    assert_eq!(models["data"].as_array().unwrap().len(), 2);
    assert_eq!(models["data"][0]["id"], "gpt-6-sol");
    assert_eq!(models["data"][1]["id"], "gpt-reserve");
    // The native Codex endpoint preserves the full upstream catalog.
    let legacy: Value = f.list("/models").await.json().await.unwrap();
    let legacy_slugs: Vec<_> = legacy["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["slug"].as_str().unwrap())
        .collect();
    assert_eq!(legacy_slugs, ["gpt-6-sol", "gpt-reserve"]);
    assert_eq!(legacy["models"][0]["catalog_marker"], "marker-gpt-6-sol");
    assert_eq!(legacy["models"][0]["use_responses_lite"], true);
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
    assert_eq!(slugs, ["gpt-6-sol", "gpt-6.1-sol", "gpt-reserve"]);
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
async fn fast_key_filters_to_canonical_fast_capable_model_ids() {
    let f = Fixture::new().await;
    f.set_release(StatusCode::OK, r#"{"tag_name":"rust-v0.159.1"}"#);
    f.app.refresh_model_version(&f.release_url).await.unwrap();
    f.refresh_catalogs().await;
    f.app.pool.set_account_models("a4", &["o3".into()]);

    for path in ["/models", "/v1/models"] {
        let normal: Value = f
            .list_with_key(path, "test-key")
            .await
            .json()
            .await
            .unwrap();
        let fast: Value = f
            .list_with_key(path, "test-key-fast")
            .await
            .json()
            .await
            .unwrap();
        let ids = |body: &Value, native: bool| {
            let (array, field) = if native {
                (&body["models"], "slug")
            } else {
                (&body["data"], "id")
            };
            array
                .as_array()
                .unwrap()
                .iter()
                .map(|model| model[field].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        let native = path == "/models";
        let normal_expected = if native {
            vec!["gpt-6-sol", "gpt-reserve"]
        } else {
            vec!["gpt-6-sol", "gpt-reserve", "o3"]
        };
        assert_eq!(ids(&normal, native), normal_expected, "{path}");
        assert_eq!(ids(&fast, native), ["gpt-6-sol", "gpt-reserve"], "{path}");
        assert!(ids(&fast, native).iter().all(|id| !id.ends_with("-fast")));
    }
}

#[tokio::test]
async fn speed_discovery_merges_account_capabilities_and_tracks_plan_lifecycle() {
    let f = Fixture::new().await;
    *f.app.model_version.write().unwrap() = Some("0.159.1".into());
    for (id, plan, tiers) in [
        (
            "a1",
            "plus",
            json!([{"id":"priority","name":"Fast","description":"Synthetic Fast capability."}]),
        ),
        (
            "a2",
            "pro",
            json!([{"id":"priority","name":"Fast","description":"Synthetic Fast capability."}]),
        ),
        (
            "a3",
            "promax",
            json!([{"id":"ultrafast","name":"Ultrafast","description":"Synthetic Ultrafast capability."}]),
        ),
        ("a4", "promax", Value::Null),
    ] {
        f.store.set_account_plan(id, plan).unwrap();
        let mut model = catalog_model("gpt-6-astra");
        model["service_tiers"] = tiers.clone();
        model["available_in_plans"] = json!(["plus", "pro", "promax"]);
        let mut models = vec![model];
        if id == "a1" {
            models.push(catalog_model("o3"));
        }
        f.app.pool.set_account_catalog(id, &models);
    }
    for (key, expected, tier) in [
        ("test-key", vec!["gpt-6-astra", "o3"], None),
        ("test-key-fast", vec!["gpt-6-astra", "o3"], Some("priority")),
        ("test-key-ultrafast", vec!["gpt-6-astra"], Some("ultrafast")),
    ] {
        for (path, field, id_field) in [("/models", "models", "slug"), ("/v1/models", "data", "id")]
        {
            let response = f.list_with_key(path, key).await;
            assert_eq!(response.status(), 200);
            let body: Value = response.json().await.unwrap();
            let models = body[field].as_array().unwrap();
            let ids: Vec<_> = models
                .iter()
                .map(|m| m[id_field].as_str().unwrap())
                .collect();
            assert_eq!(ids, expected, "{key} {path}");
            if path == "/models" {
                if let Some(tier) = tier {
                    for model in models {
                        assert_eq!(model["default_service_tier"], tier);
                        assert_eq!(model["service_tiers"].as_array().unwrap().len(), 1);
                        assert_eq!(model["service_tiers"][0]["id"], tier);
                    }
                } else {
                    assert_eq!(models[0]["service_tiers"].as_array().unwrap().len(), 2);
                }
            }
        }
    }

    // Withdraw Plus Fast; deprecated fields never grant capability.
    let mut astra = catalog_model("gpt-6-astra");
    astra["service_tiers"] = json!([]);
    astra["additional_speed_tiers"] = json!(["fast"]);
    let mut o3 = catalog_model("o3");
    o3["service_tiers"] = json!([]);
    f.app.pool.set_account_catalog("a1", &[astra, o3]);
    let fast: Value = f
        .list_with_key("/v1/models", "test-key-fast")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(fast["data"].as_array().unwrap().len(), 1);
    f.store.set_account_disabled("a3", true).unwrap();
    let ultra: Value = f
        .list_with_key("/models", "test-key-ultrafast")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(ultra["models"], json!([]));
    f.store.delete_account("a2").unwrap();
    let fast: Value = f
        .list_with_key("/models", "test-key-fast")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(fast["models"], json!([]));
    f.assert_models(&["gpt-6-astra", "o3"]).await;
}

#[tokio::test]
async fn deprecated_speed_fields_neither_grant_capability_nor_appear_in_native_discovery() {
    let f = Fixture::new().await;
    *f.app.model_version.write().unwrap() = Some("0.159.1".into());
    for id in ["a2", "a3", "a4"] {
        f.store.set_account_disabled(id, true).unwrap();
    }
    for native in [
        None,
        Some(json!([])),
        Some(Value::Null),
        Some(json!([{"id":"ultrafast"}])),
    ] {
        let mut model = catalog_model("gpt-6-astra");
        model.as_object_mut().unwrap().remove("service_tiers");
        model["additional_speed_tiers"] = json!(["fast", "ultrafast"]);
        if let Some(native) = native {
            model["service_tiers"] = native;
        }
        f.store.set_account_catalog("a1", &[model]).unwrap();
        f.app.load_cached_catalogs();

        let ordinary: Value = f.list("/models").await.json().await.unwrap();
        let model = &ordinary["models"][0];
        assert_eq!(model["slug"], "gpt-6-astra");
        assert_eq!(model["service_tiers"], json!([]));
        assert!(model.get("additional_speed_tiers").is_none());
        for key in ["test-key-fast", "test-key-ultrafast"] {
            for (path, field) in [("/models", "models"), ("/v1/models", "data")] {
                let response: Value = f.list_with_key(path, key).await.json().await.unwrap();
                assert_eq!(response[field], json!([]));
            }
        }
    }
}

#[tokio::test]
async fn a_config_model_filter_cannot_manufacture_speed_or_model_permissions() {
    let f = Fixture::with_models(vec!["gpt-6-sol".into(), "o3".into(), "invented".into()]).await;
    *f.app.model_version.write().unwrap() = Some("0.159.1".into());
    let mut model = catalog_model("gpt-6-sol");
    model["service_tiers"] = json!([]);
    f.app.pool.set_account_catalog("a1", &[model]);
    f.app.pool.set_account_models("a2", &["o3".into()]);
    f.assert_v1_models(&["gpt-6-sol", "o3"]).await;
    f.assert_codex_models(&["gpt-6-sol"]).await;
    for key in ["test-key-fast", "test-key-ultrafast"] {
        for (path, field) in [("/models", "models"), ("/v1/models", "data")] {
            let body: Value = f.list_with_key(path, key).await.json().await.unwrap();
            assert_eq!(body[field], json!([]));
        }
    }
}

#[tokio::test]
async fn withdrawn_speed_and_empty_catalogs_do_not_reappear_from_persisted_metadata() {
    let f = Fixture::new().await;
    *f.app.model_version.write().unwrap() = Some("0.159.1".into());
    for id in ["a2", "a3", "a4"] {
        f.store.set_account_disabled(id, true).unwrap();
    }
    f.store.set_account_plan("a1", "promax").unwrap();
    let account = f.store.get_account("a1").unwrap();
    let mut model = catalog_model("gpt-6-astra");
    model["service_tiers"] = json!([{"id":"ultrafast","name":"Ultrafast","description":"Synthetic Ultrafast capability."}]);
    f.upstream
        .lock()
        .unwrap()
        .catalog_overrides
        .insert("a1".into(), json!({"models":[model]}));
    f.app.refresh_models(&account).await;
    f.app.pool.set_account_catalog("a1", &[]);
    f.app.load_cached_catalogs();
    assert_eq!(
        f.app.pool.union_models_for_tier(Some("ultrafast")).unwrap(),
        ["gpt-6-astra"]
    );

    model["service_tiers"] = json!([]);
    f.upstream
        .lock()
        .unwrap()
        .catalog_overrides
        .insert("a1".into(), json!({"models":[model]}));
    f.app.refresh_models(&account).await;
    f.app.load_cached_catalogs();
    assert!(f
        .app
        .pool
        .union_models_for_tier(Some("ultrafast"))
        .unwrap()
        .is_empty());
    assert_eq!(
        f.store.account_model_catalog("a1").unwrap()[0]["service_tiers"],
        json!([])
    );

    f.upstream
        .lock()
        .unwrap()
        .catalog_overrides
        .insert("a1".into(), json!({"models":[]}));
    f.app.refresh_models(&account).await;
    f.app.load_cached_catalogs();
    assert!(f.store.account_model_catalog("a1").unwrap().is_empty());
    assert!(f.store.account_models("a1").unwrap().is_empty());
    f.assert_models(&[]).await;
}

#[tokio::test]
async fn usage_polls_keep_plan_eligibility_current_without_replacing_unknown_values() {
    let f = Fixture::new().await;
    for id in ["a2", "a3", "a4"] {
        f.store.set_account_disabled(id, true).unwrap();
    }
    let mut model = catalog_model("gpt-6-astra");
    model["available_in_plans"] = json!(["pro"]);
    f.app.pool.set_account_catalog("a1", &[model]);
    assert_eq!(
        f.app.pool.union_models_for_tier(Some("priority")).unwrap(),
        ["gpt-6-astra"]
    );
    f.upstream.lock().unwrap().usage_plan = "plus".into();
    f.app
        .fetch_usage(&f.store.get_account("a1").unwrap())
        .await
        .unwrap();
    assert_eq!(f.store.get_account("a1").unwrap().plan_type, "plus");
    assert!(f
        .app
        .pool
        .union_models_for_tier(Some("priority"))
        .unwrap()
        .is_empty());
    f.upstream.lock().unwrap().usage_plan = String::new();
    f.app
        .fetch_usage(&f.store.get_account("a1").unwrap())
        .await
        .unwrap();
    assert_eq!(f.store.get_account("a1").unwrap().plan_type, "plus");
}

#[tokio::test]
async fn malformed_catalog_responses_are_not_treated_as_a_successful_empty_catalog() {
    let f = Fixture::new().await;
    *f.app.model_version.write().unwrap() = Some("0.159.1".into());
    let account = f.store.get_account("a1").unwrap();
    f.app.refresh_models(&account).await;
    for payload in [json!({}), json!({"models":null}), json!({"models":[{}]})] {
        f.upstream
            .lock()
            .unwrap()
            .catalog_overrides
            .insert("a1".into(), payload);
        f.app.refresh_models(&account).await;
        assert!(!f.store.account_model_catalog("a1").unwrap().is_empty());
    }
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
    f.assert_v1_models(&[]).await;

    f.app
        .pool
        .set_account_models("missing", &["gpt-orphan".into()]);
    f.app.pool.set_account_models("a2", &[]);
    f.assert_v1_models(&[]).await;
    f.app.pool.set_account_models(
        "a1",
        &[
            "gpt-reserve".into(),
            "gpt-6-sol".into(),
            "gpt-reserve".into(),
        ],
    );
    f.assert_v1_models(&["gpt-6-sol", "gpt-reserve"]).await;
    assert!(f.upstream.lock().unwrap().queries.is_empty());
}

#[tokio::test]
async fn startup_restores_persisted_catalogs_without_refresh() {
    let f = Fixture::new().await;
    f.set_release(StatusCode::OK, r#"{"tag_name":"rust-v0.159.1"}"#);
    f.app.refresh_model_version(&f.release_url).await.unwrap();
    // Discovery persisted by a previous run; no upstream catalog refresh yet.
    f.store
        .set_account_catalog("a1", &[catalog_model("gpt-6-sol")])
        .unwrap();
    f.app.load_cached_catalogs();
    // Both discovery endpoints serve the restored union immediately.
    f.assert_models(&["gpt-6-sol"]).await;
    // The entitlement filter works before the first refresh: a1 provably
    // lacks reserve and is skipped for it while unknown accounts stay eligible.
    let candidates = f.app.pool.candidates("gpt-reserve").unwrap();
    assert_eq!(candidates.len(), 3);
    assert!(candidates.iter().all(|a| a.id != "a1"));
    assert!(f.upstream.lock().unwrap().queries.is_empty());
}

#[tokio::test]
async fn model_catalog_account_query_failure_returns_an_error() {
    let f = Fixture::new().await;
    f.set_release(StatusCode::OK, r#"{"tag_name":"rust-v0.159.1"}"#);
    f.app.refresh_model_version(&f.release_url).await.unwrap();
    f.app.pool.set_account_models("a1", &["gpt-6-sol".into()]);
    f.assert_v1_models(&["gpt-6-sol"]).await;

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
