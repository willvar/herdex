//! Management capability contracts: local credential identity and complete user operations.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use herdex::app::{build_router, App};
use herdex::config::{Config, ManageCfg, OAuthCfg};
use herdex::pool::{Pool, Quota};
use herdex::store::{now_secs, Account, LogEntry, RequestDiagnostics, Store};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

struct Fixture {
    app: Arc<App>,
    root: PathBuf,
    url: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    client: reqwest::Client,
}
impl Fixture {
    async fn new() -> Self {
        let upstream = axum::Router::new().route("/oauth/token", axum::routing::post(|body: axum::body::Bytes| async move {
            let form:std::collections::HashMap<String,String>=url::form_urlencoded::parse(&body).into_owned().collect();
            let code=form.get("code").map(String::as_str).unwrap_or("");
            if code=="unavailable" { return (axum::http::StatusCode::SERVICE_UNAVAILABLE,axum::Json(json!({"error":"fixture unavailable"}))); }
            let (user,workspace)=match code { "other-user"=>("user-b","workspace-a"),"other-workspace"=>("user-a","workspace-b"),_=>("user-a","workspace-a") };
            let claims=json!({"email":"shared@example.test","exp":now_secs()+3600,"https://api.openai.com/auth":{"chatgpt_account_id":workspace,"chatgpt_user_id":if code=="missing-user"{""}else{user},"chatgpt_plan_type":"pro"}});
            let token=format!("fixture.{}.fixture",URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap()));
            (axum::http::StatusCode::OK,axum::Json(json!({"access_token":"fixture-access","refresh_token":"fixture-refresh","id_token":token,"expires_in":3600})))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let upstream_task =
            tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let root =
            std::env::temp_dir().join(format!("herdex-capabilities-{}", uuid::Uuid::new_v4()));
        let store = Store::open(root.to_str().unwrap()).unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let app = Arc::new(App {
            cfg: Config {
                manage: ManageCfg {
                    key: "fixture-manage".into(),
                },
                oauth: OAuthCfg {
                    issuer: issuer.clone(),
                    client_id: "fixture-client".into(),
                },
                ..Default::default()
            },
            store: store.clone(),
            pool: Pool::new(store),
            http: client.clone(),
            usage_root: issuer,
            model_version: Default::default(),
            pending: Default::default(),
            refresh_guards: Default::default(),
            reset_guards: Default::default(),
            last_prune_day: Default::default(),
            learned_strips: Default::default(),
            routing: Default::default(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/manage/api", listener.local_addr().unwrap());
        let router = build_router(app.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            app,
            root,
            url,
            tasks: vec![task, upstream_task],
            client,
        }
    }
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.url))
            .bearer_auth("fixture-manage");
        if let Some(body) = body {
            request = request.json(&body)
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
        let status = response.status().as_u16();
        let value = response.json().await.unwrap();
        (status, value)
    }
    async fn login(&self, code: &str, expected: Option<&str>) -> (u16, Value) {
        let (_, start) = self.call(reqwest::Method::POST, "/login/start", None).await;
        let callback = format!(
            "http://localhost:1455/auth/callback?code={code}&state={}",
            start["state"].as_str().unwrap()
        );
        self.call(
            reqwest::Method::POST,
            "/login/finish",
            Some(json!({"callback_url":callback,"expected_id":expected})),
        )
        .await
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort()
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn oauth_separates_users_and_workspaces_and_preserves_disabled_policy() {
    let f = Fixture::new().await;
    let (status, first) = f.login("first", None).await;
    assert_eq!(status, 200);
    let first_id = first["id"].as_str().unwrap().to_owned();
    assert_ne!(first_id, "workspace-a");
    f.app.store.set_account_disabled(&first_id, true).unwrap();
    let (status, again) = f.login("first", Some(&first_id)).await;
    assert_eq!(status, 200);
    assert_eq!(again["id"], first_id);
    assert_eq!(again["updated"], true);
    assert_eq!(again["disabled"], true);
    let (_, other_workspace) = f.login("other-workspace", None).await;
    let (_, other_user) = f.login("other-user", None).await;
    assert_ne!(other_workspace["id"], first["id"]);
    assert_ne!(other_user["id"], first["id"]);
    assert_eq!(f.app.store.list_accounts().unwrap().len(), 3);
    let (status, error) = f.login("other-user", Some(&first_id)).await;
    assert_eq!(status, 409);
    assert_eq!(error["reauthorize_required"], true);
    assert!(f.app.store.get_account(&first_id).unwrap().disabled);
    let (status, _) = f.login("missing-user", None).await;
    assert_eq!(status, 502);
    assert_eq!(f.app.store.list_accounts().unwrap().len(), 3);
}

#[tokio::test]
async fn callback_validation_and_one_shot_failure_have_actionable_terminal_states() {
    let f = Fixture::new().await;
    let (_, start) = f.call(reqwest::Method::POST, "/login/start", None).await;
    let state = start["state"].as_str().unwrap();
    let (status,_)=f.call(reqwest::Method::POST,"/login/finish",Some(json!({"callback_url":format!("http://wrong.invalid/auth/callback?code=first&state={state}")}))).await;
    assert_eq!(status, 400);
    let payload = json!({"callback_url":format!("http://localhost:1455/auth/callback?code=unavailable&state={state}")});
    let (status, error) = f
        .call(
            reqwest::Method::POST,
            "/login/finish",
            Some(payload.clone()),
        )
        .await;
    assert_eq!(status, 502);
    assert_eq!(error["reauthorize_required"], true);
    let (status, error) = f
        .call(reqwest::Method::POST, "/login/finish", Some(payload))
        .await;
    assert_eq!(status, 400);
    assert_eq!(error["reauthorize_required"], true);
    assert!(f.app.store.list_accounts().unwrap().is_empty());
}

#[tokio::test]
async fn keys_reject_reserved_names_and_patch_is_atomic_with_empty_comment_supported() {
    let f = Fixture::new().await;
    assert_eq!(
        f.call(
            reqwest::Method::POST,
            "/keys",
            Some(json!({"key":"fixture-key","comment":"before"}))
        )
        .await
        .0,
        200
    );
    for key in ["fixture-key-fast", "fixture-key-ultrafast", "bad key"] {
        assert_ne!(
            f.call(reqwest::Method::POST, "/keys", Some(json!({"key":key})))
                .await
                .0,
            200
        );
    }
    assert_ne!(
        f.call(
            reqwest::Method::PATCH,
            "/keys/fixture-key",
            Some(json!({"key":"fixture-key-fast","comment":"must not apply","disabled":true}))
        )
        .await
        .0,
        200
    );
    let original = f.app.store.list_api_keys().unwrap().remove(0);
    assert_eq!(original.comment, "before");
    assert!(!original.disabled);
    assert_eq!(
        f.call(
            reqwest::Method::PATCH,
            "/keys/fixture-key",
            Some(json!({"comment":""}))
        )
        .await
        .0,
        200
    );
    assert_eq!(f.app.store.list_api_keys().unwrap()[0].comment, "");
    assert_ne!(
        f.call(
            reqwest::Method::PATCH,
            "/keys/not-present",
            Some(json!({"key":"not-present"}))
        )
        .await
        .0,
        200
    );
}

#[tokio::test]
async fn state_quota_is_scoped_by_local_record_and_disabled_capacity_is_not_available() {
    let f = Fixture::new().await;
    for (id, pct) in [("local-a", 10.0), ("local-b", 90.0)] {
        f.app
            .store
            .upsert_account(&Account {
                id: id.into(),
                email: "shared@example.test".into(),
                account_id: "same-workspace".into(),
                ..Default::default()
            })
            .unwrap();
        f.app.pool.observe(
            id,
            "default",
            Quota {
                primary_pct: pct,
                primary_window_secs: 18000,
                ..Default::default()
            },
        );
        f.app.store.set_account_disabled(id, true).unwrap();
    }
    let (_, state) = f.call(reqwest::Method::GET, "/state", None).await;
    assert_eq!(state["quota"]["local-a"]["default"]["primary_pct"], 10.0);
    assert_eq!(state["quota"]["local-b"]["default"]["primary_pct"], 90.0);
    assert!(state["quota"].get("shared@example.test").is_none());
    let (_, capacity) = f.call(reqwest::Method::GET, "/calibration", None).await;
    assert_eq!(capacity["active_accounts"], 0);
    assert_eq!(capacity["pool_remaining_tokens"], 0.0);
    f.app.store.set_account_disabled("local-a", false).unwrap();
    let (_, capacity) = f.call(reqwest::Method::GET, "/calibration", None).await;
    assert!(capacity["pool_remaining_tokens"].is_null());
    f.app.observe_usage(
        "local-a",
        &herdex::usage::Report {
            main: herdex::usage::Limit {
                primary: herdex::usage::Window {
                    reset_at: Some(now_secs() + 3600),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let (_, capacity) = f.call(reqwest::Method::GET, "/calibration", None).await;
    assert!(capacity["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["account_id"] == "local-a")
        .unwrap()["used_pct"]
        .is_null());
}

#[tokio::test]
async fn log_filters_cover_retained_history_before_pagination_and_keep_stream_errors() {
    let f = Fixture::new().await;
    for i in 0..125 {
        f.app.store.add_log(&LogEntry {
            ts: now_secs() - 200 + i,
            account_id: "local-a".into(),
            account_email: "a@example.test".into(),
            api_key: "fixture-key".into(),
            model: "fixture-model".into(),
            service_tier: Some(if i == 0 { "priority" } else { "" }.into()),
            status: 200,
            error: if i == 0 {
                "stream_error".into()
            } else {
                String::new()
            },
            diagnostics: Some(RequestDiagnostics {
                request_id: if i == 0 {
                    "old-request".into()
                } else {
                    format!("request-{i}")
                },
                attempt: 1,
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    assert_eq!(
        f.client
            .get(format!("{}/logs", f.url))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let (_, first) = f.call(reqwest::Method::GET, "/logs", None).await;
    let (_, third) = f.call(reqwest::Method::GET, "/logs?page=3", None).await;
    assert_eq!(first["total"], 125);
    assert_eq!(first["items"].as_array().unwrap().len(), 50);
    assert_eq!(third["items"].as_array().unwrap().len(), 25);
    let (_,filtered)=f.call(reqwest::Method::GET,"/logs?errors_only=true&account_id=local-a&model=fixture-model&service_tier=priority&request_id=old-request",None).await;
    assert_eq!(filtered["total"], 1);
    assert_eq!(filtered["items"][0]["status"], 200);
    assert_eq!(filtered["items"][0]["error"], "stream_error");
    let (_, ordinary) = f
        .call(reqwest::Method::GET, "/logs?service_tier=", None)
        .await;
    assert_eq!(ordinary["total"], 124);
    assert_eq!(
        f.call(reqwest::Method::GET, "/logs?since=20&until=10", None)
            .await
            .0,
        400
    );
}
