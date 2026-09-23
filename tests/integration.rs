//! HTTP-level integration tests: fake upstream + real axum proxy router,
//! driven through real HTTP requests (mirrors the Go proxy integration tests).

use axum::body::{Body, Bytes};
use axum::response::IntoResponse;
use axum::Router;
use herdex::app::App;
use herdex::config::Config;
use herdex::pool::Pool;
use herdex::proxy;
use herdex::store::{Account, LogEntry, Store, UsageDim};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

type StreamChunk = Result<Bytes, std::io::Error>;

#[derive(Clone)]
struct FakeUp {
    calls: Arc<Mutex<Vec<String>>>, // chatgpt-account-id seen per call
    behavior: Arc<Mutex<Behavior>>,
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
    Streaming(Arc<tokio::sync::Mutex<Option<mpsc::Receiver<StreamChunk>>>>),
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
                if let Behavior::JsonResponse(response) = &behavior {
                    return (
                        [(axum::http::header::CONTENT_TYPE, "application/json; charset=utf-8")],
                        response.clone(),
                    )
                        .into_response();
                }
                if let Behavior::Streaming(receiver) = &behavior {
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
                    return (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        Body::from_stream(stream),
                    )
                        .into_response();
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
                    Behavior::Ok | Behavior::ChunkedErrorFirst(_) => false,
                    Behavior::FailFirst(k) => n <= k as usize,
                    Behavior::AlwaysFail(_) => true,
                    // the stream-error case is a "successful" 200 whose body
                    // carries the error frame — handled below
                    Behavior::StreamErrorFirst(k) | Behavior::BomStreamErrorFirst(k) => {
                        n <= k as usize
                    }
                    Behavior::Streaming(_) | Behavior::JsonResponse(_) => unreachable!(),
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
                (
                    axum::http::StatusCode::OK,
                    format!(
                        "data: {{\"type\":\"response.in_progress\"}}\n\ndata: {{\"type\": \"response.completed\", \"response\": {{\"body\": {body}, \"usage\": {{\"input_tokens\": 120, \"input_tokens_details\": {{\"cached_tokens\": 80}}, \"output_tokens\": 45}}}}}}\n\n"
                    ),
                )
                    .into_response()
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
        behavior: Arc::new(Mutex::new(Behavior::Ok)),
    };
    let up_router = upstream.clone().router();
    let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_addr = up_listener.local_addr().unwrap();
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

fn streaming_upstream(h: &Harness) -> mpsc::Sender<StreamChunk> {
    let (sender, receiver) = mpsc::channel(4);
    *h.upstream.behavior.lock().unwrap() =
        Behavior::Streaming(Arc::new(tokio::sync::Mutex::new(Some(receiver))));
    sender
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
        .json(&serde_json::json!({"model": "gpt-5.5", "input": "hi", "stream": false}))
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
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let res = client
        .get(format!("http://{addr}/manage/panel"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "text/html; charset=utf-8");

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
