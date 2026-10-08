//! HTTP-level integration tests: fake upstream + real axum proxy router,
//! driven through real HTTP requests (mirrors the Go proxy integration tests).

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use herdex::app::App;
use herdex::config::Config;
use herdex::pool::Pool;
use herdex::proxy;
use herdex::store::{Account, LogEntry, Store, UsageDim};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

type StreamChunk = Result<Bytes, std::io::Error>;

#[derive(Clone)]
struct FakeUp {
    calls: Arc<Mutex<Vec<String>>>, // chatgpt-account-id seen per call
    bodies: Arc<Mutex<Vec<String>>>,
    behavior: Arc<Mutex<Behavior>>,
    usage: Arc<Mutex<HashMap<String, (StatusCode, Value)>>>,
    response_headers: Arc<Mutex<HeaderMap>>,
}

#[derive(Clone)]
enum Behavior {
    Ok,
    FailFirst(u16),  // first N calls fail with 429, then succeed
    AlwaysFail(u16), // always fail with the given status
    // 200 + a spaced-JSON error frame BEFORE any content: exercises the
    // buffered-prefix error detection through the full proxy path
    StreamErrorFirst(u32),
    BomStreamErrorFirst(u32),
    ChunkedErrorFirst(usize),
    JsonResponse(String),
    HttpErrorFirst(u16, String, usize),
    Streaming {
        status: u16,
        receiver: Arc<tokio::sync::Mutex<Option<mpsc::Receiver<StreamChunk>>>>,
    },
}

impl FakeUp {
    fn router(self) -> Router {
        let usage = self.usage.clone();
        Router::new().route(
            "/backend-api/wham/usage",
            axum::routing::get(move |headers: HeaderMap| async move {
                let id = headers
                    .get("Chatgpt-Account-Id")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let (status, body) = usage
                    .lock()
                    .unwrap()
                    .get(id)
                    .cloned()
                    .unwrap_or((StatusCode::BAD_GATEWAY, json!({"error": "probe unavailable"})));
                (status, axum::Json(body)).into_response()
            }),
        ).route(
            "/responses",
            axum::routing::post(move |headers: axum::http::HeaderMap, body: String| async move {
                let acct = headers
                    .get("Chatgpt-Account-Id")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                self.calls.lock().unwrap().push(acct);
                self.bodies.lock().unwrap().push(body.clone());
                let behavior = self.behavior.lock().unwrap().clone();
                let n = self.calls.lock().unwrap().len();
                if let Behavior::HttpErrorFirst(status, text, count) = &behavior {
                    if n <= *count {
                        let mut response = (StatusCode::from_u16(*status).unwrap(), text.clone()).into_response();
                        response.headers_mut().extend(self.response_headers.lock().unwrap().clone());
                        return response;
                    }
                }
                if let Behavior::JsonResponse(response) = &behavior {
                    return (
                        [(axum::http::header::CONTENT_TYPE, "application/json; charset=utf-8")],
                        response.clone(),
                    )
                        .into_response();
                }
                if let Behavior::Streaming { status, receiver } = &behavior {
                    let mut receiver = receiver
                        .lock()
                        .await
                        .take()
                        .expect("streaming request must not be retried");
                    let stream = async_stream::stream! {
                        while let Some(chunk) = receiver.recv().await {
                            yield chunk;
                        }
                    };
                    let mut response = (
                        StatusCode::from_u16(*status).unwrap(),
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        Body::from_stream(stream),
                    )
                        .into_response();
                    response.headers_mut().extend(self.response_headers.lock().unwrap().clone());
                    return response;
                }
                if let Behavior::ChunkedErrorFirst(size) = behavior {
                    if n == 1 {
                        let error = b"data: {\"type\": \"error\", \"code\": \"server_is_overloaded\", \"message\": \"the upstream is temporarily busy; try another account\"}\n\n";
                        assert!(error.len() > 65);
                        let stream = async_stream::stream! {
                            for chunk in error.chunks(size) {
                                yield Ok::<_, std::io::Error>(Bytes::copy_from_slice(chunk));
                                if size == 1 {
                                    // Flush tiny chunks separately so this exercises
                                    // the real HTTP prefix path beyond 64 reads.
                                    tokio::time::sleep(Duration::from_millis(1)).await;
                                }
                            }
                        };
                        return (
                            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                            Body::from_stream(stream),
                        ).into_response();
                    }
                }
                let fail = match behavior {
                    Behavior::Ok | Behavior::ChunkedErrorFirst(_) | Behavior::HttpErrorFirst(..) => false,
                    Behavior::FailFirst(k) => n <= k as usize,
                    Behavior::AlwaysFail(_) => true,
                    // the stream-error case is a "successful" 200 whose body
                    // carries the error frame — handled below
                    Behavior::StreamErrorFirst(k) | Behavior::BomStreamErrorFirst(k) => {
                        n <= k as usize
                    }
                    Behavior::Streaming { .. } | Behavior::JsonResponse(_) => unreachable!(),
                };
                if fail {
                    let status = match behavior {
                        Behavior::AlwaysFail(s) => s,
                        // deliberately space-formatted: only the JSON parser
                        // (not string probing) may see this error
                        Behavior::StreamErrorFirst(_) => {
                            return (
                                axum::http::StatusCode::OK,
                                "data: {\"type\": \"error\", \"code\": \"server_is_overloaded\"}\n\n".to_string(),
                            )
                                .into_response();
                        }
                        Behavior::BomStreamErrorFirst(_) => {
                            return (
                                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                                "\u{feff}data: {\"type\": \"error\", \"code\": \"slow_down\"}\n\n",
                            )
                                .into_response();
                        }
                        _ => 429,
                    };
                    return (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        format!(
                            "data: {{\"type\":\"error\",\"code\":\"slow_down\"}} call={n}\n\n"
                        ),
                    )
                        .into_response();
                }
                let mut response = (
                    axum::http::StatusCode::OK,
                    format!(
                        "data: {{\"type\":\"response.in_progress\"}}\n\ndata: {{\"type\": \"response.completed\", \"response\": {{\"body\": {body}, \"usage\": {{\"input_tokens\": 120, \"input_tokens_details\": {{\"cached_tokens\": 80}}, \"output_tokens\": 45}}}}}}\n\n"
                    ),
                )
                    .into_response();
                response
                    .headers_mut()
                    .extend(self.response_headers.lock().unwrap().clone());
                response
            }),
        )
    }
}

struct Harness {
    proxy_url: String,
    upstream: FakeUp,
    store: Store,
    pool: Pool,
    app: Arc<App>,
    upstream_task: tokio::task::JoinHandle<()>,
    tmp: String,
}

async fn harness(accounts: &[&str]) -> Harness {
    let tmp = std::env::temp_dir().join(format!(
        "herdex-it-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let store = Store::open(tmp.to_str().unwrap()).unwrap();
    store.add_api_key("sk-test", "it").unwrap();
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
        bodies: Arc::new(Mutex::new(Vec::new())),
        behavior: Arc::new(Mutex::new(Behavior::Ok)),
        usage: Default::default(),
        response_headers: Default::default(),
    };
    let up_router = upstream.clone().router();
    let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_addr = up_listener.local_addr().unwrap();
    let upstream_task =
        tokio::spawn(async move { axum::serve(up_listener, up_router).await.unwrap() });

    let cfg = Config {
        manage: herdex::config::ManageCfg {
            key: "cpm-test".into(),
        },
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
        model_version: std::sync::RwLock::new(None),
        usage_root: format!("http://{up_addr}"),
        pending: Mutex::new(Default::default()),
        last_prune_day: std::sync::atomic::AtomicI64::new(0),
        learned_strips: std::sync::Mutex::new(std::collections::HashSet::new()),
        refresh_guards: tokio::sync::Mutex::new(HashMap::new()),
        reset_guards: tokio::sync::Mutex::new(HashMap::new()),
    });
    let pr = proxy::router().with_state(app.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, pr).await.unwrap() });

    Harness {
        proxy_url: format!("http://{proxy_addr}"),
        upstream,
        store,
        pool,
        app,
        upstream_task,
        tmp: tmp.to_string_lossy().to_string(),
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.upstream_task.abort();
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

fn streaming_upstream(h: &Harness) -> mpsc::Sender<StreamChunk> {
    let (sender, receiver) = mpsc::channel(4);
    *h.upstream.behavior.lock().unwrap() = Behavior::Streaming {
        status: 200,
        receiver: Arc::new(tokio::sync::Mutex::new(Some(receiver))),
    };
    sender
}

fn usage_window(pct: i64, seconds: i64, reset: i64) -> Value {
    json!({"used_percent": pct, "limit_window_seconds": seconds, "reset_at": reset})
}

fn account_usage(plan: &str, primary: Value, secondary: Value) -> Value {
    json!({"plan_type": plan, "rate_limit": {
        "allowed": true, "limit_reached": false,
        "primary_window": primary, "secondary_window": secondary,
    }})
}

fn set_usage(h: &Harness, id: &str, payload: Value) {
    h.upstream
        .usage
        .lock()
        .unwrap()
        .insert(format!("chat-{id}"), (StatusCode::OK, payload));
}

async fn get_pool_usage(h: &Harness, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("{}{path}", h.proxy_url))
        .bearer_auth("sk-test")
        .send()
        .await
        .unwrap()
}

fn seed_weight(h: &Harness, id: &str, plan: &str, pct: f64, reset: i64, tokens: i64) {
    let now = herdex::store::now_secs();
    h.store.add_probe(id, now - 2, pct - 1.0, reset, plan);
    h.store.add_log(&LogEntry {
        account_id: id.into(),
        account_email: format!("{id}@x"),
        ts: now - 1,
        model: "gpt-5.5".into(),
        status: 200,
        input_tokens: tokens,
        ..Default::default()
    });
    h.store.add_probe(id, now, pct, reset, plan);
}

#[tokio::test]
async fn json_response_preserves_bytes_and_records_usage() {
    const RESPONSE: &str = r#"{
  "id": "resp-json",
  "object": "response",
  "output": [{"type": "message", "content": [{"type": "output_text", "text": "汉字"}]}],
  "usage": {"input_tokens": 120, "input_tokens_details": {"cached_tokens": 80}, "output_tokens": 45}
}"#;
    let h = harness(&["a1"]).await;
    *h.upstream.behavior.lock().unwrap() = Behavior::JsonResponse(RESPONSE.to_owned());
    let response = reqwest::Client::new()
        .post(format!("{}/v1/responses", h.proxy_url))
        .bearer_auth("sk-test")
        .json(&serde_json::json!({"model": "gpt-5.5", "input": "hi", "stream": false, "service_tier": "priority"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "application/json; charset=utf-8"
    );
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        RESPONSE.as_bytes()
    );
    let logs = h.store.recent_logs(5).unwrap();
    let log = logs.first().expect("JSON response must be logged");
    assert_eq!(
        (log.input_tokens, log.cached_tokens, log.output_tokens),
        (120, 80, 45)
    );
    assert!(log.error.is_empty());
    assert_eq!(log.service_tier.as_deref(), Some("priority"));
    assert_eq!(h.upstream.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn bom_stream_error_before_content_fails_over() {
    let h = harness(&["a1", "a2"]).await;
    *h.upstream.behavior.lock().unwrap() = Behavior::BomStreamErrorFirst(1);
    let response = send_post(&h, "gpt-5.5", "bom-error").await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert!(body.contains("response.completed"));
    assert!(!body.contains("slow_down"));
    assert_eq!(
        *h.upstream.calls.lock().unwrap(),
        ["chat-a1", "chat-a2"],
        "a leading UTF-8 BOM must not hide a pre-content error"
    );
}

#[tokio::test]
async fn overload_failover_is_independent_of_small_http_chunks() {
    for size in [1, usize::MAX] {
        let h = harness(&["a1", "a2"]).await;
        *h.upstream.behavior.lock().unwrap() = Behavior::ChunkedErrorFirst(size);
        let response = send_post(&h, "gpt-5.5", "tiny-error").await;
        assert_eq!(response.status(), 200);
        assert!(response
            .text()
            .await
            .unwrap()
            .contains("response.completed"));
        assert_eq!(*h.upstream.calls.lock().unwrap(), ["chat-a1", "chat-a2"]);
        let logs = h.store.recent_logs(5).unwrap();
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[1].error, "server_is_overloaded");
        assert_eq!(logs[1].account_id, "a1");
        assert!(logs[0].error.is_empty());
    }
}

#[tokio::test]
async fn sse_prefix_budget_is_local_and_independent_of_chunk_boundaries() {
    const LIMIT: usize = 2 * 1024 * 1024;
    for (content, prefix_len) in [
        (false, LIMIT),
        (false, LIMIT + 1),
        (true, LIMIT),
        (true, LIMIT + 1),
    ] {
        let ty = if content {
            "response.output_text.delta"
        } else {
            "response.completed"
        };
        let frame = |padding: &str| {
            format!(
            "data: {{\"type\":\"{ty}\",\"padding\":\"{padding}\",\"usage\":{{\"input_tokens\":120,\"cached_tokens\":80,\"output_tokens\":45}}}}\n\n"
        )
        };
        let mut wire = frame(&"x".repeat(prefix_len - frame("").len()));
        assert_eq!(wire.len(), prefix_len);
        if content {
            wire.push_str("data:{\"type\":\"response.completed\"}\n\n");
        }
        // The content event may fit exactly while the same chunk contains
        // additional bytes. EOF exactly at the budget is also allowed.
        for chunk_size in [4096, wire.len()] {
            let h = harness(&["a1", "a2"]).await;
            let sender = streaming_upstream(&h);
            let (status, received) = tokio::time::timeout(Duration::from_secs(5), async {
                let write = async {
                    for chunk in wire.as_bytes().chunks(chunk_size) {
                        if sender
                            .send(Ok(Bytes::copy_from_slice(chunk)))
                            .await
                            .is_err()
                        {
                            break; // local rejection is allowed to stop reading
                        }
                    }
                    drop(sender);
                };
                let read = async {
                    let response = send_post(&h, "gpt-5.5", "prefix-budget").await;
                    let status = response.status();
                    (status, response.bytes().await.unwrap())
                };
                tokio::join!(write, read).1
            })
            .await
            .expect("prefix budget handling must terminate");
            let logs = h.store.recent_logs(5).unwrap();
            assert_eq!(logs.len(), 1);
            assert_eq!(logs[0].account_id, "a1");
            assert_eq!(h.upstream.calls.lock().unwrap().len(), 1);
            assert_eq!(
                h.pool.select("gpt-5.5", "").unwrap().len(),
                2,
                "local limit must not cool either account"
            );
            if prefix_len > LIMIT {
                assert_eq!(status, 502);
                assert_eq!(logs[0].status, 502);
                assert_eq!(logs[0].error, "prefix_limit_exceeded");
                let body: serde_json::Value = serde_json::from_slice(&received).unwrap();
                assert_eq!(body["error"]["code"], "prefix_limit_exceeded");
                assert!(h.pool.pinned("prefix-budget", "gpt-5.5").is_none());
            } else {
                assert_eq!(status, 200);
                assert_eq!(received.as_ref(), wire.as_bytes());
                assert!(logs[0].error.is_empty());
                assert_eq!(
                    (
                        logs[0].input_tokens,
                        logs[0].cached_tokens,
                        logs[0].output_tokens
                    ),
                    (120, 80, 45)
                );
            }
        }
    }
}

#[tokio::test]
async fn json_larger_than_sse_prefix_budget_preserves_bytes_and_usage() {
    let h = harness(&["a1"]).await;
    let wire = serde_json::json!({
        "object": "response", "output": "x".repeat(2 * 1024 * 1024),
        "usage": {"input_tokens": 120, "input_tokens_details": {"cached_tokens": 80}, "output_tokens": 45}
    }).to_string();
    *h.upstream.behavior.lock().unwrap() = Behavior::JsonResponse(wire.clone());
    let response = send_post(&h, "gpt-5.5", "large-json").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), wire.as_bytes());
    let logs = h.store.recent_logs(5).unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(
        (
            logs[0].input_tokens,
            logs[0].cached_tokens,
            logs[0].output_tokens
        ),
        (120, 80, 45)
    );
    assert!(logs[0].error.is_empty());
}

#[tokio::test]
async fn oversized_json_is_forwarded_without_cooling_or_retrying() {
    let h = harness(&["a1", "a2"]).await;
    let wire = serde_json::json!({
        "output": "x".repeat(8 * 1024 * 1024),
        "usage": {"input_tokens": 120, "output_tokens": 45}
    })
    .to_string();
    *h.upstream.behavior.lock().unwrap() = Behavior::JsonResponse(wire.clone());
    let response = send_post(&h, "gpt-5.5", "oversized-json").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), wire.as_bytes());
    let logs = h.store.recent_logs(5).unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].error, "observer_limit_exceeded");
    assert_eq!(h.pool.select("gpt-5.5", "").unwrap().len(), 2);
}

#[tokio::test]
async fn oversized_sse_observation_resumes_at_the_next_event() {
    // Both a single unfinished line and many data lines in one event need
    // their own bounded path. Neither may hide the following completion.
    for multiline in [false, true] {
        let h = harness(&["a1", "a2"]).await;
        let sender = streaming_upstream(&h);
        let first = b"data:{\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n";
        sender.send(Ok(Bytes::from_static(first))).await.unwrap();
        let response = send_post(&h, "gpt-5.5", "oversized-event").await;
        assert_eq!(response.status(), 200);
        let padding = "x".repeat(8192);
        let mut oversized = String::from("data:{\"padding\":\"");
        for _ in 0..1025 {
            oversized.push_str(&padding);
            if multiline {
                oversized.push_str("\r\ndata: ");
            }
        }
        oversized.push_str("\"}\r\n\r\n");
        let completion = b"data:{\"type\":\"response.completed\",\"usage\":{\"input_tokens\":120,\"cached_tokens\":80,\"output_tokens\":45}}\r\n\r\n";
        let actual = tokio::time::timeout(Duration::from_secs(10), async {
            let write = async {
                // Cuts include CRLF and oversized-line boundaries.
                for chunk in oversized.as_bytes().chunks(4093) {
                    sender
                        .send(Ok(Bytes::copy_from_slice(chunk)))
                        .await
                        .unwrap();
                }
                sender
                    .send(Ok(Bytes::from_static(completion)))
                    .await
                    .unwrap();
                drop(sender);
            };
            let read = async { response.bytes().await.unwrap() };
            tokio::join!(write, read).1
        })
        .await
        .unwrap();
        assert_eq!(
            actual.as_ref(),
            [
                first.as_slice(),
                oversized.as_bytes(),
                completion.as_slice()
            ]
            .concat()
        );
        let logs = h.store.recent_logs(5).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].error, "observer_limit_exceeded");
        assert_eq!(
            (
                logs[0].input_tokens,
                logs[0].cached_tokens,
                logs[0].output_tokens
            ),
            (120, 80, 45)
        );
        assert_eq!(h.pool.select("gpt-5.5", "").unwrap().len(), 2);
    }
}

#[tokio::test]
async fn split_content_is_forwarded_before_upstream_eof() {
    let h = harness(&["a1"]).await;
    let sender = streaming_upstream(&h);
    let first = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hel";
    let second = b"lo\"}\n\n";
    sender.send(Ok(Bytes::from_static(first))).await.unwrap();
    sender.send(Ok(Bytes::from_static(second))).await.unwrap();

    // The sender stays open until the client receives the complete first
    // event. Waiting for EOF (or a later event) would deadlock this exchange.
    let expected = [first.as_slice(), second.as_slice()].concat();
    let mut response = tokio::time::timeout(
        Duration::from_secs(3),
        send_post(&h, "gpt-5.5", "split-content"),
    )
    .await
    .expect("complete content event must release response headers before EOF");
    let mut actual = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        while actual.len() < expected.len() {
            actual.extend_from_slice(&response.chunk().await.unwrap().expect("premature EOF"));
        }
    })
    .await
    .expect("split content event must reach the client before EOF");
    assert_eq!(actual, expected);
    drop(sender);
    assert!(response.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn large_completion_preserves_bytes_and_usage_after_streaming_starts() {
    let h = harness(&["a1"]).await;
    let sender = streaming_upstream(&h);
    let first = b"data:{\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n";
    sender.send(Ok(Bytes::from_static(first))).await.unwrap();
    let mut response = tokio::time::timeout(
        Duration::from_secs(3),
        send_post(&h, "gpt-5.5", "large-completion"),
    )
    .await
    .expect("first content must be forwarded while upstream stays open");
    let mut actual = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        while actual.len() < first.len() {
            actual.extend_from_slice(&response.chunk().await.unwrap().expect("premature EOF"));
        }
    })
    .await
    .expect("client must receive the first content before completion is sent");
    assert_eq!(actual, first);

    let completion = format!(
        "data: {}\n\n",
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "汉".repeat(30_000)}]}],
                "usage": {"input_tokens": 120, "input_tokens_details": {"cached_tokens": 80}, "output_tokens": 45}
            }
        })
    );
    assert!(completion.len() > 64 * 1024);
    // 4096-byte cuts also split multibyte UTF-8 characters. Feed this only
    // after the first event reached the client, exercising the streaming path.
    assert!(completion
        .as_bytes()
        .chunks(4096)
        .any(|chunk| std::str::from_utf8(chunk).is_err()));
    let expected = [first.as_slice(), completion.as_bytes()].concat();
    tokio::time::timeout(Duration::from_secs(3), async {
        let write = async {
            for chunk in completion.as_bytes().chunks(4096) {
                sender
                    .send(Ok(Bytes::copy_from_slice(chunk)))
                    .await
                    .unwrap();
            }
            drop(sender);
        };
        let read = async {
            actual.extend_from_slice(&response.bytes().await.unwrap());
        };
        tokio::join!(write, read);
    })
    .await
    .expect("large completed event must finish without stalling");
    assert_eq!(actual, expected, "SSE bytes must pass through unchanged");
    let logs = h.store.recent_logs(5).unwrap();
    let log = logs.first().expect("completed request must be logged");
    assert_eq!(
        (log.input_tokens, log.cached_tokens, log.output_tokens),
        (120, 80, 45)
    );
    assert!(log.error.is_empty());
    assert_eq!(h.upstream.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn coalesced_content_then_error_is_forwarded_without_retry() {
    let h = harness(&["a1", "a2"]).await;
    let sender = streaming_upstream(&h);
    let events = b"data: {\"type\": \"response.output_text.delta\", \"delta\": \"hi\"}\n\ndata:{\"type\": \"error\", \"code\": \"slow_down\"}\n\n";
    sender.send(Ok(Bytes::from_static(events))).await.unwrap();
    drop(sender);
    let response = tokio::time::timeout(
        Duration::from_secs(3),
        send_post(&h, "gpt-5.5", "content-error"),
    )
    .await
    .expect("content followed by error must be forwarded");
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), events);
    assert_eq!(h.upstream.calls.lock().unwrap().len(), 1);
    let logs = h.store.recent_logs(5).unwrap();
    assert_eq!(
        logs.first().expect("error must be logged").error,
        "slow_down"
    );
    assert_eq!(h.pool.select("gpt-5.5", "").unwrap()[0].id, "a2");
}

#[tokio::test]
async fn upstream_disconnect_logs_and_cools_before_returning_body_error() {
    for last_event in ["token_count", "response.completed"] {
        let h = harness(&["a1", "a2"]).await;
        let sender = streaming_upstream(&h);
        let prefix = format!(
            "data:{{\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}}\n\ndata:{}\n\n",
            serde_json::json!({"type": last_event, "usage": {
                "input_tokens": 120, "cached_tokens": 80, "output_tokens": 45
            }})
        );
        sender.send(Ok(Bytes::from(prefix.clone()))).await.unwrap();
        let mut response = send_post(&h, "gpt-5.5", "disconnect").await;
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), async {
            while received.len() < prefix.len() {
                received.extend_from_slice(&response.chunk().await.unwrap().unwrap());
            }
        })
        .await
        .unwrap();
        assert_eq!(received, prefix.as_bytes());

        sender
            .send(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "test upstream disconnected",
            )))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(3), response.bytes())
                .await
                .unwrap()
                .is_err()
        );
        let logs = h.store.recent_logs(5).unwrap();
        assert_eq!(
            logs.len(),
            1,
            "transport failure must finalize exactly once"
        );
        assert_eq!(logs[0].error, "upstream_stream_error");
        let diagnostics = logs[0].diagnostics.as_ref().unwrap();
        assert_eq!(diagnostics.stage, "stream");
        assert_eq!(diagnostics.attempt, 1);
        assert_eq!(diagnostics.received_bytes, prefix.len() as u64);
        assert!(diagnostics.content_seen);
        assert_eq!(diagnostics.completed, last_event == "response.completed");
        assert!(
            diagnostics.error_detail.contains("error reading a body"),
            "{}",
            diagnostics.error_detail
        );
        assert!(
            diagnostics.error_detail.contains("unexpected EOF"),
            "must retain hyper's underlying transport cause: {}",
            diagnostics.error_detail
        );
        assert!(diagnostics.attempt_ms <= diagnostics.elapsed_ms);
        assert!(!diagnostics.error_detail.contains("http://"));
        assert_eq!(
            (
                logs[0].input_tokens,
                logs[0].cached_tokens,
                logs[0].output_tokens
            ),
            (120, 80, 45)
        );
        let candidates = h.pool.select("gpt-5.5", "").unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, "a2");
        assert_eq!(h.upstream.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn dropped_stream_records_observed_state_once() {
    for (last_event, expected_error, eligible_accounts) in [
        ("token_count", "client_cancelled", 2),
        ("response.completed", "", 2),
        ("error", "slow_down", 1),
    ] {
        let h = harness(&["a1", "a2"]).await;
        let sender = streaming_upstream(&h);
        let prefix = format!(
            "data:{{\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}}\n\ndata:{}\n\n",
            serde_json::json!({
                "type": last_event, "code": "slow_down",
                "usage": {"input_tokens": 120, "cached_tokens": 80, "output_tokens": 45}
            })
        );
        sender.send(Ok(Bytes::from(prefix.clone()))).await.unwrap();
        let mut response = send_post(&h, "gpt-5.5", "cancelled").await;
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut received = Vec::new();
            while received.len() < prefix.len() {
                received.extend_from_slice(&response.chunk().await.unwrap().unwrap());
            }
            assert_eq!(received, prefix.as_bytes());
        })
        .await
        .unwrap();
        // Keep the upstream channel open: only client cancellation can end
        // this response. Closing the upstream proves cancellation propagated.
        drop(response);
        tokio::time::timeout(Duration::from_secs(3), sender.closed())
            .await
            .expect("dropping the client body must cancel its upstream");
        let logs = h.store.recent_logs(5).unwrap();
        assert_eq!(logs.len(), 1, "{last_event}: finalize exactly once");
        assert_eq!(logs[0].error, expected_error, "{last_event}");
        assert_eq!(
            (
                logs[0].input_tokens,
                logs[0].cached_tokens,
                logs[0].output_tokens
            ),
            (120, 80, 45),
            "{last_event}: retain already observed usage"
        );
        assert_eq!(
            h.pool.select("gpt-5.5", "").unwrap().len(),
            eligible_accounts,
            "{last_event}: cancellation alone must not cool the account"
        );
    }
}

#[tokio::test]
async fn passthrough_swaps_auth_and_preserves_body() {
    let h = harness(&["a1"]).await;
    let res = send_post(&h, "gpt-5.5", "s1").await;
    assert_eq!(res.status(), 200);
    let text = res.text().await.unwrap();
    // the request body must survive the round trip inside the SSE frame
    // (JSON formatting may differ upstream, so compare whitespace-insensitively)
    let compact = text.replace(' ', "");
    assert!(
        compact.contains(r#""body":{"input":"hi","model":"gpt-5.5"}"#),
        "body mutated: {text}"
    );
    assert_eq!(h.upstream.calls.lock().unwrap()[0], "chat-a1");
}

#[tokio::test]
async fn request_tiers_are_recorded_without_mutating_the_body_or_tokens() {
    let h = harness(&["a1"]).await;
    let client = reqwest::Client::new();
    for (path, tier) in [
        ("/v1/responses", None),
        ("/v1/responses", Some(serde_json::Value::Null)),
        ("/v1/responses", Some(serde_json::json!("default"))),
        ("/v1/responses", Some(serde_json::json!("priority"))),
        ("/v1/responses", Some(serde_json::json!(true))),
        (
            "/backend-api/codex/responses",
            Some(serde_json::json!("flex")),
        ),
    ] {
        let mut body = serde_json::json!({"model": "gpt-5.5", "input": "hi"});
        if let Some(tier) = tier {
            body["service_tier"] = tier;
        }
        let expected = match body.get("service_tier") {
            None | Some(serde_json::Value::Null) => Some(""),
            Some(value) => value.as_str(),
        };
        let response = client
            .post(format!("{}{path}", h.proxy_url))
            .bearer_auth("sk-test")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = response.text().await.unwrap();
        let completed: serde_json::Value = bytes
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
            .find(|event| event["type"] == "response.completed")
            .unwrap();
        assert_eq!(completed["response"]["body"], body);
        let logs = h.store.recent_logs(1).unwrap();
        assert_eq!(logs[0].service_tier.as_deref(), expected);
        assert_eq!(
            (
                logs[0].input_tokens,
                logs[0].cached_tokens,
                logs[0].output_tokens
            ),
            (120, 80, 45)
        );
    }
}

#[tokio::test]
async fn fast_tier_survives_stream_failover_on_every_logged_attempt() {
    let h = harness(&["a1", "a2"]).await;
    *h.upstream.behavior.lock().unwrap() = Behavior::StreamErrorFirst(1);
    let response = reqwest::Client::new()
        .post(format!("{}/v1/responses", h.proxy_url))
        .bearer_auth("sk-test")
        .json(&serde_json::json!({"model":"gpt-5.5", "input":"hi", "service_tier":"priority"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    let logs = h.store.recent_logs(5).unwrap();
    assert_eq!(logs.len(), 2);
    assert!(logs
        .iter()
        .all(|l| l.service_tier.as_deref() == Some("priority")));
    assert_eq!(logs[1].error, "server_is_overloaded");
    assert_eq!(logs[1].diagnostics.as_ref().unwrap().stage, "prefix");
    assert_eq!(logs[1].diagnostics.as_ref().unwrap().attempt, 1);
    assert_eq!(logs[0].diagnostics.as_ref().unwrap().attempt, 2);
    assert_eq!(
        logs[1].diagnostics.as_ref().unwrap().request_id,
        logs[0].diagnostics.as_ref().unwrap().request_id
    );
    assert!(logs[0].error.is_empty());
}

#[tokio::test]
async fn fast_model_alias_rewrites_model_and_forces_priority() {
    let h = harness(&["a1"]).await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/responses", h.proxy_url))
        .bearer_auth("sk-test")
        .json(&json!({
            "model": "gpt-5.5-fast",
            "input": "hi",
            "service_tier": "default"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.text().await.unwrap();

    let bodies = h.upstream.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    let forwarded: Value = serde_json::from_str(&bodies[0]).unwrap();
    assert_eq!(forwarded["model"], "gpt-5.5");
    assert_eq!(forwarded["service_tier"], "priority");
    assert_eq!(forwarded["input"], "hi");
    drop(bodies);

    let logs = h.store.recent_logs(5).unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].model, "gpt-5.5");
    assert_eq!(logs[0].service_tier.as_deref(), Some("priority"));
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
async fn spaced_stream_error_before_content_fails_over() {
    // a 200 stream whose error frame uses spaced JSON ("type": "error")
    // must STILL fail over to the next account, not just get logged —
    // the regression this guards: JSON parser wired to the tail but not
    // to the pre-content failover path
    let h = harness(&["a1", "a2"]).await;
    *h.upstream.behavior.lock().unwrap() = Behavior::StreamErrorFirst(1);
    let res = send_post(&h, "gpt-5.5", "s-spaced").await;
    assert_eq!(res.status(), 200, "must fail over and succeed via a2");
    let calls = h.upstream.calls.lock().unwrap().clone();
    assert_eq!(calls[0], "chat-a1");
    assert_eq!(
        calls[1], "chat-a2",
        "spaced error frame must trigger failover"
    );
    res.bytes().await.unwrap();
    let logs = h.store.recent_logs(5).unwrap();
    assert_eq!(logs.len(), 2, "each upstream attempt gets exactly one log");
    assert_eq!(logs[0].account_email, "a2@x");
    assert!(logs[0].error.is_empty());
    assert_eq!(logs[1].account_email, "a1@x");
    assert_eq!(logs[1].error, "server_is_overloaded");
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
async fn failed_http_attempts_are_logged_and_linked_without_recording_error_bodies() {
    for status in [400, 401, 429, 503] {
        let h = harness(&["a1", "a2"]).await;
        let error = json!({"error": {"code": "test_upstream_error", "message": "private request content at-a1 sk-test"}}).to_string();
        *h.upstream.behavior.lock().unwrap() = Behavior::HttpErrorFirst(status, error.clone(), 1);
        h.upstream
            .response_headers
            .lock()
            .unwrap()
            .insert("x-request-id", "upstream-test-123".parse().unwrap());
        let response = send_post(&h, "gpt-5.5", "http-errors").await;
        assert_eq!(response.status(), if status == 400 { 400 } else { 200 });
        let request_id = response.headers()["x-herdex-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let body = response.text().await.unwrap();
        if status == 400 {
            assert_eq!(body, error);
        }
        let logs = h.store.recent_logs(10).unwrap();
        assert_eq!(logs.len(), if status == 400 { 1 } else { 2 });
        let failed = logs.last().unwrap();
        assert_eq!(failed.status, status as i64);
        assert_eq!(failed.error, "test_upstream_error");
        let diagnostics = failed.diagnostics.as_ref().unwrap();
        assert_eq!(diagnostics.attempt, 1);
        assert_eq!(diagnostics.stage, "http_body");
        assert_eq!(
            diagnostics.upstream_request_id.as_deref(),
            Some("upstream-test-123")
        );
        let serialized = serde_json::to_string(diagnostics).unwrap();
        for secret in ["private request content", "at-a1", "sk-test"] {
            assert!(!serialized.contains(secret));
        }
        assert!(logs
            .iter()
            .all(|row| row.diagnostics.as_ref().unwrap().request_id == request_id));
        if status != 400 {
            assert_eq!(logs[0].diagnostics.as_ref().unwrap().attempt, 2);
            assert!(logs[0].error.is_empty());
        }
        assert_eq!((failed.input_tokens, failed.output_tokens), (0, 0));
    }
}

#[tokio::test]
async fn connection_failures_keep_the_underlying_cause_and_all_attempts() {
    let mut h = harness(&["a1", "a2"]).await;
    h.upstream_task.abort();
    assert!((&mut h.upstream_task).await.unwrap_err().is_cancelled());
    let response = send_post(&h, "gpt-5.5", "connect-error").await;
    assert_eq!(response.status(), 502);
    let request_id = response.headers()["x-herdex-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    response.bytes().await.unwrap();
    let logs = h.store.recent_logs(5).unwrap();
    assert_eq!(logs.len(), 2);
    for (i, row) in logs.iter().rev().enumerate() {
        assert_eq!(row.status, 0, "no HTTP response was received");
        assert_eq!(row.error, "upstream_request_error");
        let diagnostics = row.diagnostics.as_ref().unwrap();
        assert_eq!(diagnostics.request_id, request_id);
        assert_eq!(diagnostics.attempt, i as u32 + 1);
        assert_eq!(diagnostics.stage, "connect");
        assert!(diagnostics
            .error_detail
            .to_lowercase()
            .contains("connection refused"));
        assert!(!diagnostics.error_detail.contains("http://"));
        assert!(!diagnostics.error_detail.contains("at-a"));
    }
}

#[tokio::test]
async fn parameter_retry_has_its_own_attempt_number_and_preserves_fast_tier() {
    let h = harness(&["a1"]).await;
    *h.upstream.behavior.lock().unwrap() = Behavior::HttpErrorFirst(
        400,
        json!({"detail":"Unsupported parameter: max_output_tokens"}).to_string(),
        1,
    );
    let response = reqwest::Client::new().post(format!("{}/v1/responses", h.proxy_url))
        .bearer_auth("sk-test")
        .json(&json!({"model":"gpt-5.5","input":"hi","max_output_tokens":10,"service_tier":"priority"}))
        .send().await.unwrap();
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    let logs = h.store.recent_logs(5).unwrap();
    assert_eq!(logs.len(), 2);
    assert_eq!(logs[1].status, 400);
    assert_eq!(logs[1].error, "http_400");
    assert_eq!(logs[0].status, 200);
    assert_eq!(logs[0].diagnostics.as_ref().unwrap().attempt, 2);
    assert_eq!(logs[1].diagnostics.as_ref().unwrap().attempt, 1);
    assert_eq!(
        logs[0].diagnostics.as_ref().unwrap().request_id,
        logs[1].diagnostics.as_ref().unwrap().request_id
    );
    assert!(logs
        .iter()
        .all(|row| row.service_tier.as_deref() == Some("priority")));
}

#[tokio::test]
async fn disconnects_before_content_and_during_http_error_bodies_are_recorded_once() {
    for status in [200, 503] {
        let h = harness(&["a1"]).await;
        let sender = streaming_upstream(&h);
        if let Behavior::Streaming { status: code, .. } = &mut *h.upstream.behavior.lock().unwrap()
        {
            *code = status;
        }
        // A quota observation confirms the proxy has received these headers,
        // so the disconnect cannot race into the request-header phase.
        h.upstream.response_headers.lock().unwrap().insert(
            "x-codex-bengalfox-primary-used-percent",
            "12".parse().unwrap(),
        );
        let prefix = b"data:{\"type\":\"response.in_progress\"}\n\n";
        sender.send(Ok(Bytes::from_static(prefix))).await.unwrap();
        let response = tokio::time::timeout(Duration::from_secs(3), async {
            let read = send_post(&h, "gpt-5.5", "prefix-disconnect");
            let disconnect = async {
                while h.pool.observation("a1", "gpt-5.5").is_none() {
                    tokio::task::yield_now().await;
                }
                sender
                    .send(Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "test disconnect",
                    )))
                    .await
                    .unwrap();
            };
            tokio::join!(read, disconnect).0
        })
        .await
        .expect("disconnect must terminate the attempt");
        assert_eq!(response.status(), if status == 200 { 502 } else { 503 });
        let request_id = response.headers()["x-herdex-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        response.bytes().await.unwrap();
        let logs = h.store.recent_logs(5).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].status, status as i64);
        assert_eq!(
            logs[0].error,
            if status == 200 {
                "upstream_stream_error"
            } else {
                "upstream_error_body_error"
            }
        );
        let diagnostics = logs[0].diagnostics.as_ref().unwrap();
        assert_eq!(diagnostics.request_id, request_id);
        assert_eq!(
            diagnostics.stage,
            if status == 200 { "prefix" } else { "http_body" }
        );
        assert_eq!(diagnostics.attempt, 1);
        assert_eq!(diagnostics.received_bytes, prefix.len() as u64);
        assert!(!diagnostics.content_seen);
        assert!(!diagnostics.completed);
        assert!(
            diagnostics.error_detail.contains("error reading a body"),
            "{}",
            diagnostics.error_detail
        );
    }
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
async fn pool_usage_separates_weekly_and_monthly_weights_and_resets() {
    let h = harness(&["a1", "a2", "a3"]).await;
    let now = herdex::store::now_secs();
    let week_reset = now + 2 * 86400;
    let month_reset = now + 20 * 86400;
    for (id, plan, pct, seconds, reset, tokens) in [
        ("a1", "pro", 20, 604800, week_reset, 300),
        ("a2", "prolite", 40, 604800, now + 4 * 86400, 100),
        ("a3", "free", 11, 2592000, month_reset, 10000),
    ] {
        seed_weight(&h, id, plan, pct as f64, reset, tokens);
        set_usage(
            &h,
            id,
            account_usage(plan, usage_window(pct, seconds, reset), Value::Null),
        );
    }
    for path in ["/api/codex/usage", "/v1/api/codex/usage", "/wham/usage"] {
        let response = get_pool_usage(&h, path).await;
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        let p = &body["rate_limit"]["primary_window"];
        let s = &body["rate_limit"]["secondary_window"];
        assert_eq!(p["limit_window_seconds"], 604800);
        assert_eq!(p["used_percent"], 25); // 20*300 + 40*100, monthly weight excluded
        assert_eq!(p["reset_at"], week_reset);
        assert_eq!(s["limit_window_seconds"], 2592000);
        assert_eq!(s["used_percent"], 11);
        assert_eq!(s["reset_at"], month_reset);
        assert_eq!(body["plan_type"], "pro"); // same virtual plan as whoami
        assert_eq!(body["account_id"], proxy::POOL_IDENTITY.account_id);
        assert_eq!(body["user_id"], proxy::POOL_IDENTITY.user_id);
        for window in [p, s] {
            for key in [
                "used_percent",
                "limit_window_seconds",
                "reset_at",
                "reset_after_seconds",
            ] {
                assert!(i32::try_from(window[key].as_i64().unwrap()).is_ok());
            }
            let reset = window["reset_at"].as_i64().unwrap();
            let after = window["reset_after_seconds"].as_i64().unwrap();
            assert!((reset - herdex::store::now_secs() - after).abs() <= 1);
        }
    }
}

#[tokio::test]
async fn pool_usage_preserves_secondary_windows_and_extra_periods() {
    let h = harness(&["a1", "a2", "a3"]).await;
    let reset = herdex::store::now_secs() + 86400;
    set_usage(
        &h,
        "a1",
        account_usage(
            "pro",
            usage_window(30, 18000, reset),
            usage_window(70, 604800, reset + 1),
        ),
    );
    set_usage(
        &h,
        "a2",
        account_usage(
            "prolite",
            usage_window(50, 604800, reset + 2),
            usage_window(10, 18000, reset + 3),
        ),
    );
    set_usage(
        &h,
        "a3",
        account_usage("free", usage_window(11, 2592000, reset + 4), Value::Null),
    );
    let body: Value = get_pool_usage(&h, "/api/codex/usage")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["rate_limit"]["primary_window"]["limit_window_seconds"],
        18000
    );
    assert_eq!(body["rate_limit"]["primary_window"]["used_percent"], 20);
    assert_eq!(
        body["rate_limit"]["secondary_window"]["limit_window_seconds"],
        604800
    );
    assert_eq!(body["rate_limit"]["secondary_window"]["used_percent"], 60);
    let extra = body["additional_rate_limits"].as_array().unwrap();
    assert_eq!(extra.len(), 1);
    assert!(extra[0]["metered_feature"].as_str().is_some());
    assert!(extra[0]["limit_name"].as_str().is_some());
    assert_eq!(
        extra[0]["rate_limit"]["primary_window"]["limit_window_seconds"],
        2592000
    );
    assert_eq!(extra[0]["rate_limit"]["primary_window"]["used_percent"], 11);
    h.upstream
        .response_headers
        .lock()
        .unwrap()
        .insert("x-codex-primary-used-percent", "30".parse().unwrap());
    let response = send_post(&h, "gpt-5.5", "extra-periods").await;
    assert_eq!(response.status(), 200);
    for (prefix, window) in [
        (
            "x-codex-primary".to_owned(),
            &body["rate_limit"]["primary_window"],
        ),
        (
            "x-codex-secondary".to_owned(),
            &body["rate_limit"]["secondary_window"],
        ),
        (
            format!(
                "x-{}-primary",
                extra[0]["metered_feature"]
                    .as_str()
                    .unwrap()
                    .replace('_', "-")
            ),
            &extra[0]["rate_limit"]["primary_window"],
        ),
    ] {
        for (suffix, expected) in [
            ("used-percent", window["used_percent"].as_i64().unwrap()),
            (
                "window-minutes",
                window["limit_window_seconds"].as_i64().unwrap() / 60,
            ),
            ("reset-at", window["reset_at"].as_i64().unwrap()),
        ] {
            assert_eq!(
                response.headers()[format!("{prefix}-{suffix}")]
                    .to_str()
                    .unwrap()
                    .parse::<i64>()
                    .unwrap(),
                expected,
            );
        }
    }
    response.bytes().await.unwrap();
}

#[tokio::test]
async fn pool_usage_secondary_only_window_matches_the_response_headers() {
    let h = harness(&["a1"]).await;
    let reset = herdex::store::now_secs() + 86400;
    set_usage(
        &h,
        "a1",
        account_usage("prolite", Value::Null, usage_window(40, 604800, reset)),
    );
    let body: Value = get_pool_usage(&h, "/api/codex/usage")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["rate_limit"]["primary_window"]["used_percent"], 40);
    h.upstream
        .response_headers
        .lock()
        .unwrap()
        .insert("x-codex-secondary-used-percent", "40".parse().unwrap());
    let response = send_post(&h, "gpt-5.5", "secondary-only").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-codex-primary-used-percent"], "40");
    assert_eq!(
        response.headers()["x-codex-primary-window-minutes"],
        "10080"
    );
    assert!(!response
        .headers()
        .contains_key("x-codex-secondary-used-percent"));
    response.bytes().await.unwrap();
}

#[tokio::test]
async fn pool_usage_with_only_free_accounts_keeps_a_real_monthly_window() {
    let h = harness(&["a1"]).await;
    let reset = herdex::store::now_secs() + 86400;
    set_usage(
        &h,
        "a1",
        account_usage("free", usage_window(11, 2592000, reset), Value::Null),
    );
    let body: Value = get_pool_usage(&h, "/api/codex/usage")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["rate_limit"]["primary_window"]["limit_window_seconds"],
        2592000
    );
    assert_eq!(body["rate_limit"]["primary_window"]["used_percent"], 11);
    assert!(body["rate_limit"]["secondary_window"].is_null());
}

#[tokio::test]
async fn pool_usage_does_not_turn_missing_windows_or_failed_probes_into_zero_usage() {
    let h = harness(&["a1", "a2", "a3"]).await;
    let reset = herdex::store::now_secs() + 86400;
    set_usage(
        &h,
        "a1",
        account_usage(
            "pro",
            json!({"limit_window_seconds": 604800, "reset_at": reset}),
            Value::Null,
        ),
    );
    set_usage(
        &h,
        "a2",
        account_usage(
            "free",
            json!({"used_percent": 11, "reset_at": reset}),
            Value::Null,
        ),
    );
    assert_eq!(get_pool_usage(&h, "/api/codex/usage").await.status(), 502);
    set_usage(
        &h,
        "a3",
        account_usage("prolite", usage_window(40, 604800, reset), Value::Null),
    );
    let body: Value = get_pool_usage(&h, "/api/codex/usage")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["rate_limit"]["primary_window"]["used_percent"], 40);
    assert!(body["rate_limit"]["secondary_window"].is_null());
    // A subsequent turn must not reintroduce missing percentages as 0% via
    // the pool snapshots populated by these same partial probe responses.
    h.upstream
        .response_headers
        .lock()
        .unwrap()
        .insert("x-codex-primary-used-percent", "40".parse().unwrap());
    let response = send_post(&h, "gpt-5.5", "partial-usage").await;
    assert_eq!(response.headers()["x-codex-primary-used-percent"], "40");
    assert!(!response
        .headers()
        .contains_key("x-codex-secondary-used-percent"));
    response.bytes().await.unwrap();
    h.upstream.usage.lock().unwrap().remove("chat-a3");
    assert_eq!(get_pool_usage(&h, "/api/codex/usage").await.status(), 502);
}

#[tokio::test]
async fn pool_usage_remains_allowed_when_another_period_has_available_accounts() {
    let h = harness(&["a1", "a2"]).await;
    let reset = herdex::store::now_secs() + 86400;
    let mut weekly = account_usage("pro", usage_window(100, 604800, reset), Value::Null);
    weekly["rate_limit"]["allowed"] = json!(false);
    weekly["rate_limit"]["limit_reached"] = json!(true);
    set_usage(&h, "a1", weekly);
    set_usage(
        &h,
        "a2",
        account_usage("free", usage_window(11, 2592000, reset), Value::Null),
    );
    let body: Value = get_pool_usage(&h, "/api/codex/usage")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["rate_limit"]["primary_window"]["used_percent"], 100);
    assert_eq!(body["rate_limit"]["secondary_window"]["used_percent"], 11);
    assert_eq!(body["rate_limit"]["allowed"], true);
    assert_eq!(body["rate_limit"]["limit_reached"], false);
}

#[tokio::test]
async fn pool_usage_headers_match_poll_periods_without_touching_model_limits_or_body() {
    let h = harness(&["a1", "a2", "a3", "disabled", "deleted"]).await;
    let now = herdex::store::now_secs();
    let week_reset = now + 2 * 86400;
    let month_reset = now + 20 * 86400;
    for (id, plan, pct, seconds, reset) in [
        ("a1", "pro", 20, 604800, week_reset),
        ("a2", "prolite", 40, 604800, week_reset + 86400),
        ("a3", "free", 11, 2592000, month_reset),
        ("disabled", "free", 99, 2592000, now + 1),
        ("deleted", "pro", 99, 604800, now + 1),
    ] {
        let report = account_usage(plan, usage_window(pct, seconds, reset), Value::Null);
        h.app.observe_usage(
            id,
            &herdex::usage::parse(&serde_json::to_vec(&report).unwrap()).unwrap(),
        );
        set_usage(&h, id, report);
    }
    h.store.set_account_disabled("disabled", true).unwrap();
    h.store.delete_account("deleted").unwrap();
    let polled: Value = get_pool_usage(&h, "/api/codex/usage")
        .await
        .json()
        .await
        .unwrap();
    {
        let mut headers = h.upstream.response_headers.lock().unwrap();
        for (name, value) in [
            ("x-codex-primary-used-percent", "11".to_string()),
            ("x-codex-primary-window-minutes", "43200".to_string()),
            ("x-codex-primary-reset-at", month_reset.to_string()),
            ("x-codex-primary-reset-after-seconds", "1".to_string()),
            ("x-codex-secondary-used-percent", "99".to_string()),
            ("x-codex-plan-type", "free".to_string()),
            ("x-codex-bengalfox-primary-used-percent", "88".to_string()),
            (
                "x-codex-bengalfox-primary-window-minutes",
                "300".to_string(),
            ),
        ] {
            headers.insert(name, value.parse().unwrap());
        }
    }
    h.pool.pin("period-headers", "a3", "gpt-5.5");
    let response = send_post(&h, "gpt-5.5", "period-headers").await;
    assert_eq!(response.status(), 200);
    assert_eq!(h.upstream.calls.lock().unwrap()[0], "chat-a3");
    for slot in ["primary", "secondary"] {
        let window = &polled["rate_limit"][format!("{slot}_window")];
        for (suffix, expected) in [
            ("used-percent", window["used_percent"].as_i64().unwrap()),
            (
                "window-minutes",
                window["limit_window_seconds"].as_i64().unwrap() / 60,
            ),
            ("reset-at", window["reset_at"].as_i64().unwrap()),
        ] {
            let name = format!("x-codex-{slot}-{suffix}");
            assert_eq!(
                response.headers()[&name]
                    .to_str()
                    .unwrap()
                    .parse::<i64>()
                    .unwrap(),
                expected
            );
        }
        let after = response.headers()[format!("x-codex-{slot}-reset-after-seconds")]
            .to_str()
            .unwrap()
            .parse::<i64>()
            .unwrap();
        assert!(
            (window["reset_at"].as_i64().unwrap() - herdex::store::now_secs() - after).abs() <= 1
        );
    }
    assert_eq!(response.headers()["x-codex-plan-type"], "pro");
    assert_eq!(
        response.headers()["x-codex-bengalfox-primary-used-percent"],
        "88"
    );
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("\"body\": {\"input\":\"hi\",\"model\":\"gpt-5.5\"}"));
}

#[tokio::test]
async fn pool_usage_headers_do_not_fall_back_to_a_single_account_when_unknown() {
    let h = harness(&["a1"]).await;
    h.upstream.response_headers.lock().unwrap().extend([
        (
            "x-codex-primary-used-percent"
                .parse::<axum::http::HeaderName>()
                .unwrap(),
            "33".parse().unwrap(),
        ),
        (
            "x-codex-primary-window-minutes"
                .parse::<axum::http::HeaderName>()
                .unwrap(),
            "43200".parse().unwrap(),
        ),
    ]);
    let response = send_post(&h, "gpt-5.5", "unknown-pool").await;
    assert_eq!(response.status(), 200);
    assert!(!response
        .headers()
        .contains_key("x-codex-primary-used-percent"));
    assert!(!response
        .headers()
        .contains_key("x-codex-primary-window-minutes"));
    response.bytes().await.unwrap();
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
            let by_acc = h
                .store
                .usage_by(herdex::store::UsageDim::Account, 7)
                .unwrap();
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
async fn pat_workspace_discovery_matches_identity_and_checks_credentials() {
    let h = harness(&["a1"]).await;
    let client = reqwest::Client::new();
    let identity: serde_json::Value = client
        .get(format!("{}/v1/user-auth-credential/whoami", h.proxy_url))
        .bearer_auth("sk-test")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for path in [
        "/api/codex/accounts/check",
        "/backend-api/wham/accounts/check",
    ] {
        let url = format!("{}{path}", h.proxy_url);
        let response = client
            .get(&url)
            .bearer_auth("sk-test")
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            200,
            "{path}: workspace discovery must stay local"
        );
        let discovery: serde_json::Value = response.json().await.unwrap();
        let accounts = discovery["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0]["id"], identity["chatgpt_account_id"]);
        // NO_CONSTRAINT retains the configured HTTPS gateway origin rather
        // than routing the gateway credential to a real OpenAI workspace.
        assert_eq!(accounts[0]["workspace_backend_origin"], "NO_CONSTRAINT");
        assert_eq!(accounts[0]["account_routing_override"], "NO_CONSTRAINT");
        assert_eq!(
            discovery["default_account_id"],
            identity["chatgpt_account_id"]
        );
        assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
        assert_eq!(
            client
                .get(&url)
                .bearer_auth("invalid")
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
    }
    h.store.set_api_key_disabled("sk-test", true).unwrap();
    let response = client
        .get(format!("{}/api/codex/accounts/check", h.proxy_url))
        .bearer_auth("sk-test")
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        401,
        "disabled keys cannot discover a workspace"
    );
}

#[tokio::test]
async fn full_router_assembles_and_serves_panel() {
    let h = harness(&["a1"]).await;
    let root = herdex::app::build_router({
        // rebuild App identical to harness but reuse store/pool
        Arc::new(App {
            cfg: Config {
                manage: herdex::config::ManageCfg {
                    key: "cpm-test".into(),
                },
                ..Default::default()
            },
            store: h.store.clone(),
            pool: h.pool.clone(),
            http: reqwest::Client::new(),
            model_version: Default::default(),
            usage_root: "https://chatgpt.com".into(),
            pending: Mutex::new(Default::default()),
            last_prune_day: std::sync::atomic::AtomicI64::new(0),
            learned_strips: std::sync::Mutex::new(std::collections::HashSet::new()),
            refresh_guards: tokio::sync::Mutex::new(HashMap::new()),
            reset_guards: tokio::sync::Mutex::new(HashMap::new()),
        })
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, root).await.unwrap() });

    // panel must be served (route conflict would have panicked in build_router)
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let res = client
        .get(format!("http://{addr}/manage/panel"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 307);
    assert_eq!(res.headers()["location"], "/manage/panel/");

    let res = client
        .get(format!("http://{addr}/manage/panel/"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "text/html; charset=utf-8");

    // Removing the explicit route must not expose the retired URL through
    // the static handler's former index fallback.
    let res = client
        .get(format!("http://{addr}/manage/panel/legacy"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);

    // Client routes still work through the same router; no release has been
    // discovered in this fixture, so both discovery endpoints fail closed.
    for path in ["/v1/models", "/models"] {
        let res = reqwest::Client::new()
            .get(format!("http://{addr}{path}"))
            .bearer_auth("sk-test")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 503);
    }
}

#[test]
fn usage_aggregates_by_dimension() {
    let dir = std::env::temp_dir().join(format!(
        "herdex-usage-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let st = Store::open(dir.to_str().unwrap()).unwrap();
    let now = herdex::store::now_secs();
    for (acct, key, model, inp, out) in [
        ("a@x", "k1", "m1", 100, 10),
        ("a@x", "k1", "m2", 50, 5),
        ("b@x", "k2", "m1", 200, 20),
    ] {
        st.add_log(&LogEntry {
            ts: now,
            account_id: acct.into(),
            account_email: acct.into(),
            api_key: key.into(),
            model: model.into(),
            service_tier: None,
            status: 200,
            latency_ms: 1,
            input_tokens: inp,
            cached_tokens: 0,
            output_tokens: out,
            error: String::new(),
            diagnostics: None,
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
    assert_eq!(
        by_model.iter().find(|r| r.label == "m1").unwrap().requests,
        2
    );
    let daily = st.usage_daily(7).unwrap();
    assert_eq!(daily.len(), 1);
    assert_eq!(daily[0].requests, 3);
    assert_eq!(daily[0].input + daily[0].output, 385);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prune_logs_respects_retention() {
    let dir = std::env::temp_dir().join(format!(
        "herdex-prune-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let st = Store::open(dir.to_str().unwrap()).unwrap();
    let now = herdex::store::now_secs();
    for (ts, acct) in [
        (now - 800 * 86400, "old@x"),
        (now - 100 * 86400, "mid@x"),
        (now, "new@x"),
    ] {
        st.add_log(&LogEntry {
            ts,
            account_id: acct.into(),
            account_email: acct.into(),
            api_key: "k".into(),
            model: "m".into(),
            service_tier: None,
            status: 200,
            latency_ms: 1,
            input_tokens: 1,
            cached_tokens: 0,
            output_tokens: 1,
            error: String::new(),
            diagnostics: None,
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
    let dir = std::env::temp_dir().join(format!(
        "herdex-cfg-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("c.toml");
    std::fs::write(&p, "[manage]\nkey = \"k\"\n").unwrap();
    assert_eq!(
        herdex::config::load(p.to_str().unwrap())
            .unwrap()
            .retention_days(),
        730
    );
    std::fs::write(&p, "retention-days = 0\n[manage]\nkey = \"k\"\n").unwrap();
    assert_eq!(
        herdex::config::load(p.to_str().unwrap())
            .unwrap()
            .retention_days(),
        0
    );
    let _ = std::fs::remove_dir_all(&dir);
}
