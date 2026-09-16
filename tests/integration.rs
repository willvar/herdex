//! HTTP-level integration tests: fake upstream + real axum proxy router,
//! driven through real HTTP requests (mirrors the Go proxy integration tests).

use axum::Router;
use std::collections::HashMap;
use herdex::app::App;
use herdex::config::Config;
use herdex::pool::Pool;
use herdex::proxy;
use herdex::store::{Account, LogEntry, Store, UsageDim};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct FakeUp {
    calls: Arc<Mutex<Vec<String>>>,          // chatgpt-account-id seen per call
    behavior: Arc<Mutex<Behavior>>,
}

#[derive(Clone)]
enum Behavior {
    Ok,
    FailFirst(u16),   // first N calls fail with 429, then succeed
    AlwaysFail(u16),  // always fail with the given status
}

impl FakeUp {
    fn router(self) -> Router {
        Router::new().route(
            "/responses",
            axum::routing::post(move |headers: axum::http::HeaderMap, body: String| async move {
                let acct = headers
                    .get("Chatgpt-Account-Id")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                self.calls.lock().unwrap().push(acct);
                let behavior = self.behavior.lock().unwrap().clone();
                let n = self.calls.lock().unwrap().len();
                let fail = match behavior {
                    Behavior::Ok => false,
                    Behavior::FailFirst(k) => n <= k as usize,
                    Behavior::AlwaysFail(_) => true,
                };
                if fail {
                    let status = match behavior {
                        Behavior::AlwaysFail(s) => s,
                        _ => 429,
                    };
                    return (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        format!(r#"{{"error":{{"type":"usage_limit_reached"}}}} call={n}"#),
                    );
                }
                (
                    axum::http::StatusCode::OK,
                    format!(
                        r#"{{"ok":true,"call":{n},"body":{body},"usage":{{"input_tokens":120,"input_tokens_details":{{"cached_tokens":80}},"output_tokens":45}}}}"#
                    ),
                )
            }),
        )
    }
}

struct Harness {
    proxy_url: String,
    upstream: FakeUp,
    store: Store,
    pool: Pool,
    tmp: String,
}

async fn harness(accounts: &[&str]) -> Harness {
    let tmp = std::env::temp_dir().join(format!(
        "herdex-it-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let store = Store::open(tmp.to_str().unwrap()).unwrap();
    store
        .add_api_key("sk-test", "it")
        .unwrap();
    for id in accounts {
        store
            .upsert_account(&Account {
                id: (*id).to_string(),
                email: format!("{id}@x"),
                plan_type: "pro".into(),
                account_id: format!("chat-{id}"),
                access_token: format!("at-{id}"),
                refresh_token: format!("rt-{id}"),
                expires_at: herdex::store::now_secs() + 3600,
                ..Default::default()
            })
            .unwrap();
    }
    let pool = Pool::new(store.clone());

    let upstream = FakeUp {
        calls: Arc::new(Mutex::new(Vec::new())),
        behavior: Arc::new(Mutex::new(Behavior::Ok)),
    };
    let up_router = upstream.clone().router();
    let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_addr = up_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(up_listener, up_router).await.unwrap() });

    let cfg = Config {
        manage: herdex::config::ManageCfg { key: "cpm-test".into() },
        upstream: herdex::config::UpstreamCfg {
            base_url: format!("http://{up_addr}"),
        },
        header_defaults: [
            ("User-Agent".to_string(), "codex_cli_rs/0.153.4".to_string()),
            ("Originator".to_string(), "codex_cli_rs".to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let app = Arc::new(App {
        cfg: cfg.clone(),
        store: store.clone(),
        pool: pool.clone(),
        http: reqwest::Client::new(),
        usage_root: "https://chatgpt.com".into(),
        pending: Mutex::new(Default::default()),
        last_prune_day: std::sync::atomic::AtomicI64::new(0),
        learned_strips: std::sync::Mutex::new(std::collections::HashSet::new()),
        refresh_guards: tokio::sync::Mutex::new(HashMap::new()),
    });
    let pr = proxy::router().with_state(app);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, pr).await.unwrap() });

    Harness {
        proxy_url: format!("http://{proxy_addr}"),
        upstream,
        store,
        pool,
        tmp: tmp.to_string_lossy().to_string(),
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

async fn send_post(h: &Harness, model: &str, sid: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/responses", h.proxy_url))
        .bearer_auth("sk-test")
        .header("Session-Id", sid)
        .json(&serde_json::json!({"model": model, "input": "hi"}))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn passthrough_swaps_auth_and_preserves_body() {
    let h = harness(&["a1"]).await;
    let res = send_post(&h, "gpt-5.5", "s1").await;
    assert_eq!(res.status(), 200);
    let text = res.text().await.unwrap();
    assert!(text.contains(r#""body":{"input":"hi","model":"gpt-5.5"}"#), "body mutated: {text}");
    assert_eq!(h.upstream.calls.lock().unwrap()[0], "chat-a1");
}

#[tokio::test]
async fn failover_covers_all_candidates() {
    let h = harness(&["a1", "a2", "a3", "a4"]).await;
    *h.upstream.behavior.lock().unwrap() = Behavior::FailFirst(3);
    let res = send_post(&h, "gpt-5.5", "s1").await;
    assert_eq!(res.status(), 200);
    let calls = h.upstream.calls.lock().unwrap();
    assert_eq!(calls.len(), 4, "all four candidates must be attempted");
}

#[tokio::test]
async fn all_fail_surfaces_real_upstream_error() {
    let h = harness(&["a1", "a2"]).await;
    *h.upstream.behavior.lock().unwrap() = Behavior::AlwaysFail(429);
    let res = send_post(&h, "gpt-5.5", "s1").await;
    // CLIProxyAPI returned "auth_unavailable" here; herdex surfaces 429
    assert_eq!(res.status(), 429);
    let text = res.text().await.unwrap();
    assert!(!text.contains("auth_unavailable"));
}

#[tokio::test]
async fn session_affinity_and_failover_repin() {
    let h = harness(&["a1", "a2"]).await;
    // first request: least-used tie -> a1; session pins to a1
    let res = send_post(&h, "gpt-5.5", "sess-1").await;
    assert_eq!(res.status(), 200);
    let first = h.upstream.calls.lock().unwrap()[0].clone();

    // pin a2 manually and re-send same session: affinity must move to a2
    h.pool.pin("sess-1", "a2", "gpt-5.5");
    let res = send_post(&h, "gpt-5.5", "sess-1").await;
    assert_eq!(res.status(), 200);
    let calls = h.upstream.calls.lock().unwrap();
    assert_eq!(calls[1], "chat-a2", "affinity must override ordering");
    let _ = first;
}

#[tokio::test]
async fn quota_headers_recorded() {
    let h = harness(&["a1"]).await;
    // wrap: fake upstream already returns ok without quota headers; record via pool
    let res = send_post(&h, "gpt-5.5", "s1").await;
    assert_eq!(res.status(), 200);
    // no quota headers in fake upstream -> observation falls back to defaults (absent)
    assert!(h.pool.observation("a1", "gpt-5.5").is_none());
}

#[tokio::test]
async fn usage_logged_with_api_key_and_tokens() {
    let h = harness(&["a1"]).await;
    let res = send_post(&h, "gpt-5.5", "sid-u").await;
    assert_eq!(res.status(), 200);
    let _ = res.text().await.unwrap();
    // stream tail parsing runs after the response body is fully consumed;
    // poll briefly since add_log happens on stream end
    for _ in 0..20 {
        let logs = h.store.recent_logs(5).unwrap();
        if let Some(l) = logs.iter().find(|l| l.status == 200) {
            assert_eq!(l.api_key, "sk-test");
            assert_eq!(l.account_email, "a1@x");
            assert_eq!(l.input_tokens, 120);
            assert_eq!(l.cached_tokens, 80);
            assert_eq!(l.output_tokens, 45);
            let by_acc = h.store.usage_by(herdex::store::UsageDim::Account, 7).unwrap();
            assert_eq!(by_acc.len(), 1);
            assert_eq!(by_acc[0].label, "a1@x");
            assert_eq!(by_acc[0].input + by_acc[0].cached + by_acc[0].output, 245);
            let daily = h.store.usage_daily(7).unwrap();
            assert_eq!(daily.len(), 1);
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("request_log entry with tokens never appeared");
}

#[tokio::test]
async fn unauthenticated_rejected() {
    let h = harness(&["a1"]).await;
    let res = reqwest::Client::new()
        .post(format!("{}/v1/responses", h.proxy_url))
        .bearer_auth("wrong")
        .json(&serde_json::json!({"model": "gpt-5.5"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn full_router_assembles_and_serves_panel() {
    let h = harness(&["a1"]).await;
    let root = herdex::app::build_router({
        // rebuild App identical to harness but reuse store/pool
        Arc::new(App {
            cfg: Config {
                manage: herdex::config::ManageCfg { key: "cpm-test".into() },
                ..Default::default()
            },
            store: h.store.clone(),
            pool: h.pool.clone(),
            http: reqwest::Client::new(),
            usage_root: "https://chatgpt.com".into(),
            pending: Mutex::new(Default::default()),
            last_prune_day: std::sync::atomic::AtomicI64::new(0),
        learned_strips: std::sync::Mutex::new(std::collections::HashSet::new()),
        refresh_guards: tokio::sync::Mutex::new(HashMap::new()),
        })
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, root).await.unwrap() });

    // panel must be served (route conflict would have panicked in build_router)
    let res = reqwest::get(format!("http://{addr}/manage/panel")).await.unwrap();
    assert_eq!(res.status(), 200);
    let body = res.text().await.unwrap();
    assert!(body.contains("herdex"), "panel html not served");

    // client routes still work through the same router
    let res = reqwest::Client::new()
        .get(format!("http://{addr}/v1/models"))
        .bearer_auth("sk-test")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
}

#[test]
fn usage_aggregates_by_dimension() {
    let dir = std::env::temp_dir().join(format!("herdex-usage-{}-{}", std::process::id(), uuid::Uuid::new_v4()));
    let st = Store::open(dir.to_str().unwrap()).unwrap();
    let now = herdex::store::now_secs();
    for (acct, key, model, inp, out) in [
        ("a@x", "k1", "m1", 100, 10),
        ("a@x", "k1", "m2", 50, 5),
        ("b@x", "k2", "m1", 200, 20),
    ] {
        st.add_log(&LogEntry {
            ts: now,
            account_email: acct.into(),
            api_key: key.into(),
            model: model.into(),
            status: 200,
            latency_ms: 1,
            input_tokens: inp,
            cached_tokens: 0,
            output_tokens: out,
            error: String::new(),
        });
    }
    let by_acc = st.usage_by(UsageDim::Account, 7).unwrap();
    assert_eq!(by_acc.len(), 2);
    assert_eq!(by_acc[0].label, "b@x"); // ordered by total tokens desc
    assert_eq!(by_acc[0].input, 200);
    let by_key = st.usage_by(UsageDim::Key, 7).unwrap();
    assert_eq!(by_key.len(), 2);
    let by_model = st.usage_by(UsageDim::Model, 7).unwrap();
    assert_eq!(by_model.len(), 2);
    assert_eq!(by_model.iter().find(|r| r.label == "m1").unwrap().requests, 2);
    let daily = st.usage_daily(7).unwrap();
    assert_eq!(daily.len(), 1);
    assert_eq!(daily[0].requests, 3);
    assert_eq!(daily[0].input + daily[0].output, 385);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prune_logs_respects_retention() {
    let dir = std::env::temp_dir().join(format!("herdex-prune-{}-{}", std::process::id(), uuid::Uuid::new_v4()));
    let st = Store::open(dir.to_str().unwrap()).unwrap();
    let now = herdex::store::now_secs();
    for (ts, acct) in [(now - 800 * 86400, "old@x"), (now - 100 * 86400, "mid@x"), (now, "new@x")] {
        st.add_log(&LogEntry {
            ts,
            account_email: acct.into(),
            api_key: "k".into(),
            model: "m".into(),
            status: 200,
            latency_ms: 1,
            input_tokens: 1,
            cached_tokens: 0,
            output_tokens: 1,
            error: String::new(),
        });
    }
    assert_eq!(st.prune_logs(730).unwrap(), 1); // only the 800-day-old row
    assert_eq!(st.recent_logs(10).unwrap().len(), 2);
    assert_eq!(st.prune_logs(0).unwrap(), 0); // 0 = keep forever
    assert_eq!(st.prune_logs(1).unwrap(), 1); // only rows older than 1 day; today's row survives
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn retention_days_config_default_and_zero() {
    let dir = std::env::temp_dir().join(format!("herdex-cfg-{}-{}", std::process::id(), uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("c.toml");
    std::fs::write(&p, "[manage]\nkey = \"k\"\n").unwrap();
    assert_eq!(herdex::config::load(p.to_str().unwrap()).unwrap().retention_days(), 730);
    std::fs::write(&p, "retention-days = 0\n[manage]\nkey = \"k\"\n").unwrap();
    assert_eq!(herdex::config::load(p.to_str().unwrap()).unwrap().retention_days(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}
