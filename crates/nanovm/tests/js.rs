//! End-to-end tests of JavaScript apps: `js/tests/app.js` built into a
//! component by the JS runtime and composed with the key-value provider
//! (`tools/build-app.sh js/tests/app.js -o js/target/test-app.wasm`),
//! served by a real in-process node.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use nanovm::config::{AppConfig, NodeConfig};
use nanovm::table::Node;

#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;
use load::{Reply, request};

fn wasm() -> Vec<u8> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../js/target/test-app.wasm");
    std::fs::read(&p).unwrap_or_else(|_| panic!("{} is missing: run tools/build-app.sh js/tests/app.js -o js/target/test-app.wasm", p.display()))
}

fn app() -> AppConfig {
    let limits = serde_json::json!({
        "memory_mb": 32, "cpu_ms": 500, "request_timeout_ms": 2000, "post_return_timeout_ms": 1000,
        "idle_timeout_ms": 60000, "disk_mb": 1, "max_files": 100, "egress": ["localhost"],
    });
    AppConfig {
        name: "js".into(),
        digest: "test".into(),
        env: Default::default(),
        config: [("greeting".to_string(), "hello from config".to_string())].into(),
        secrets: Default::default(),
        limits: serde_json::from_value(limits).unwrap(),
        worker: Default::default(),
    }
}

fn node_cfg(test: &str, max_live: usize) -> NodeConfig {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let dir = root.join("nodes").join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cache = root.join("cwasm-cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::os::unix::fs::symlink(&cache, dir.join("cache")).unwrap();
    let cfg = NodeConfig { data_dir: dir, max_live_objects: max_live, memory_reservation_mb: 64, dev: true, ..NodeConfig::default() };
    cfg.validate().unwrap();
    cfg
}

fn node_with(cfg: NodeConfig, app: AppConfig) -> Arc<Node> {
    nanovm::engine::preflight(&cfg).unwrap();
    Node::new(cfg, &app, &wasm(), None).unwrap()
}

fn node(test: &str, max_live: usize) -> Arc<Node> {
    node_with(node_cfg(test, max_live), app())
}

async fn get(node: &Arc<Node>, id: &str, path: &str) -> Reply {
    request(node, "GET", id, path, "").await
}

async fn post(node: &Arc<Node>, id: &str, path: &str) -> Reply {
    request(node, "POST", id, path, "").await
}

async fn settle(node: &Arc<Node>) {
    let t0 = Instant::now();
    while !node.phases().keys().all(|p| *p == "idle") {
        assert!(t0.elapsed() < Duration::from_secs(2), "objects never went idle");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Routing, `env.id`, module state for the instance's life, and the
/// object's storage outliving it.
#[tokio::test(flavor = "multi_thread")]
async fn counter_persists_across_eviction() {
    let node = node("js_counter_persists", 2);
    for n in 1..=3 {
        let r = post(&node, "a", "/incr").await;
        assert_eq!(r.status, 200, "{:?} {}", r.error, r.body);
        assert_eq!((r.json()["n"].as_u64(), r.json()["hits"].as_u64()), (Some(n), Some(n)));
    }
    // `a` is idle (its post-return commit done) before `b` is used, so it
    // is the least recently used: a read of `b` can finish first.
    settle(&node).await;
    let r = get(&node, "b", "/").await;
    assert_eq!((r.json()["object"].as_str(), r.json()["n"].as_u64()), (Some("b"), Some(0)), "objects are separate");
    settle(&node).await;
    get(&node, "c", "/").await;
    assert!(node.metrics.evicted_lru.load(Relaxed) >= 1);
    settle(&node).await;
    let r = get(&node, "a", "/").await;
    assert_eq!(r.json()["n"], 3, "storage kept");
    assert_eq!(r.json()["hits"], 1, "fresh instance: module state is the instance's");
    assert_eq!(get(&node, "a", "/nope").await.status, 404);
}

/// An exception fails the request, and its writes roll back.
#[tokio::test(flavor = "multi_thread")]
async fn exception_rolls_back_writes() {
    let node = node("js_exception_rolls_back", 8);
    assert_eq!(post(&node, "x", "/incr").await.json()["n"], 1);
    let r = post(&node, "x", "/incr-then-throw").await;
    assert_eq!((r.status, r.error.as_deref()), (500, Some("guest-error")), "{}", r.body);
    assert!(r.body.contains("Uncaught Error: boom"), "{}", r.body);
    let r = get(&node, "x", "/").await;
    assert_eq!((r.json()["n"].as_u64(), r.json()["hits"].as_u64()), (Some(1), Some(1)), "rolled back, new instance");
    let r = get(&node, "x", "/never").await;
    assert_eq!(r.status, 500, "{}", r.body);
    assert!(r.body.contains("can never settle"), "{}", r.body);
}

/// A runaway loop and a runaway allocation are killed by the app's CPU and
/// memory limits, and runaway recursion by the stack's; the object then
/// serves from a fresh instance.
#[tokio::test(flavor = "multi_thread")]
async fn limits_kill_runaway_javascript() {
    let mut app = app();
    // Headroom for the allocation to reach the memory limit on a busy host.
    app.limits.cpu_ms = 300;
    let node = node_with(node_cfg("js_limits", 8), app);
    for (path, reason) in [("/spin", "cpu"), ("/alloc", "memory"), ("/recurse", "trap")] {
        assert_eq!(post(&node, "r", "/incr").await.status, 200);
        let t0 = Instant::now();
        let r = get(&node, "r", path).await;
        assert_eq!((r.status, r.error.as_deref()), (500, Some(reason)), "{path}: {}", r.body);
        assert!(t0.elapsed() < Duration::from_millis(1500), "{path} took {:?}", t0.elapsed());
        let after = get(&node, "r", "/").await;
        assert_eq!(after.status, 200, "{}", after.body);
        assert_eq!(after.json()["hits"], 1, "{path}: instance was replaced");
    }
    assert_eq!(get(&node, "r", "/").await.json()["n"], 3, "storage survives kills");
}

/// `fetch()` goes through the host, under the app's egress policy.
#[tokio::test(flavor = "multi_thread")]
async fn outbound_fetch_obeys_egress() {
    let node = node("js_egress", 8);
    let r = get(&node, "f", "/fetch?url=http://169.254.169.254/latest/meta-data/").await;
    assert_eq!(r.status, 502, "{}", r.body);
    assert!(r.json()["denied"].as_str().unwrap().contains("HttpRequestDenied"), "{}", r.body);
    // Listed, but loopback: only the IP filter can stop it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let r = get(&node, "f", &format!("/fetch?url=http://localhost:{port}/")).await;
    assert!(r.json()["denied"].as_str().unwrap().contains("DestinationIpProhibited"), "{}", r.body);
    assert!(node.metrics.egress_denied.load(Relaxed) >= 2);
}

/// An allowed `fetch()` round-trips status and body.
#[tokio::test(flavor = "multi_thread")]
async fn outbound_fetch_round_trips() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(nanovm::server::serve_http1(listener, |req| async move {
        nanovm::object::error_response(201, "upstream", req.uri().path())
    }));
    let mut app = app();
    app.limits.egress.push(format!("127.0.0.1:{port}"));
    let node = node_with(node_cfg("js_fetch_round_trips", 8), app);
    let r = get(&node, "f", &format!("/fetch?url=http://127.0.0.1:{port}/hello")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["status"], 201);
    assert!(r.json()["body"].as_str().unwrap().contains("/hello"), "{}", r.body);
    server.abort();
}

/// `fetch()` rejects an answer whose body the upstream cut short (fewer
/// bytes than its content-length, or a chunked body with no last chunk),
/// rather than resolving with the part that came as if it were all of it.
/// It goes through the Rust SDK's `Fetch::send`.
#[tokio::test(flavor = "multi_thread")]
async fn review_fetch_rejects_a_truncated_upstream_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let (mut got, mut buf) = (vec![], [0; 4096]);
                while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                    }
                }
                let answer: &[u8] = match String::from_utf8_lossy(&got).split(' ').nth(1) {
                    Some("/short") => b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nonly ten b",
                    Some("/cut") => b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n",
                    _ => b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nwhole",
                };
                let _ = socket.write_all(answer).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                // Dropped: the connection closes there.
            });
        }
    });
    let mut app = app();
    app.limits.egress.push(format!("127.0.0.1:{port}"));
    let node = node_with(node_cfg("js_fetch_truncated", 8), app);
    let r = get(&node, "f", &format!("/fetch?url=http://127.0.0.1:{port}/whole")).await;
    assert_eq!((r.status, &r.json()["body"]), (200, &serde_json::json!("whole")), "{}", r.body);
    for path in ["/short", "/cut"] {
        let r = get(&node, "f", &format!("/fetch?url=http://127.0.0.1:{port}{path}")).await;
        assert_eq!(r.status, 502, "{path}: a part of a body is not the body: {}", r.body);
        assert!(r.json()["denied"].as_str().unwrap().contains("ErrorCode::"), "{path}: {}", r.body);
    }
    server.abort();
}

/// `env.objects.get(id).fetch()` calls another object, which commits first.
#[tokio::test(flavor = "multi_thread")]
async fn objects_call_objects() {
    let node = node("js_objects_call", 8);
    let r = post(&node, "a", "/call?id=b").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!((r.json()["status"].as_u64(), r.json()["callee"]["n"].as_u64()), (Some(200), Some(1)));
    assert_eq!(get(&node, "b", "/").await.json()["n"], 1, "the callee's storage has it");
    assert_eq!(get(&node, "a", "/").await.json()["n"], 0, "the caller's doesn't");
    let r = post(&node, "a", "/call?id=a").await;
    assert_eq!(r.json()["status"], 508, "a cycle is refused: {}", r.body);
}

/// Requests and responses, Web APIs, timers, `waitUntil` and config.
#[tokio::test(flavor = "multi_thread")]
async fn web_apis() {
    let node = node("js_web_apis", 8);
    let req = http::Request::post("http://localhost/echo?a=1&b=x%20y")
        .header("content-type", "text/plain")
        .body(bytes::Bytes::from("hi there"))
        .unwrap();
    let resp = node.dispatch("w", req).await;
    assert_eq!((resp.status().as_u16(), resp.headers()["x-echo"].to_str().unwrap()), (201, "1"));
    let body = http_body_util::BodyExt::collect(resp.into_body()).await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"method": "POST", "path": "/echo", "q": {"a": "1", "b": "x y"}, "type": "text/plain", "body": "hi there"})
    );
    let r = get(&node, "w", "/web").await;
    assert_eq!(
        r.json(),
        serde_json::json!({
            "headers": [["a", "1, 2"], ["b", "3"]], "text": "héllo", "len": 6, "uuid": true, "b64": "aGk=",
            "clone": {"a": [1, {"b": 2}]}, "form": "q=a+b&x=%26",
            "url": {
                "parts": [
                    "https://user:pw@example.com:8443/b/c?y=2&z=3#h", "https://example.com:8443", "example.com:8443",
                    "/b/c", "?y=2&z=3", "#h", "user", "8443",
                ],
                "after": "https://user:pw@example.com/q%20r?y=2&z=3", "all": ["1", "2"], "bad": false,
            },
            "decoded": "h\u{fffd}", "fatal": "TypeError", "cloned": [true, 5], "atob": "hi",
        }),
        "{}",
        r.body
    );
    let r = get(&node, "w", "/timers").await;
    assert_eq!(
        (&r.json()["order"], &r.json()["ticks"]),
        (&serde_json::json!(["a", "b"]), &serde_json::json!(3)),
        "{}",
        r.body
    );
    assert!(r.json()["waited"].as_u64().unwrap() >= 30, "{}", r.body);
    let r = get(&node, "w", "/config").await;
    assert_eq!(r.json(), serde_json::json!({"greeting": "hello from config"}));
    let r = get(&node, "w", "/later").await;
    assert_eq!((r.status, r.body.as_str()), (200, "answered first"));
    let r = get(&node, "w", "/later-value").await;
    assert_eq!(r.json()["later"], 5, "waitUntil work finished and committed before the next request");
    assert_eq!(post(&node, "w", "/put-bad").await.json()["error"], "TypeError", "kv values are JSON");
    let resp =
        node.dispatch("w", http::Request::get("http://localhost/length").body(bytes::Bytes::new()).unwrap()).await;
    let lengths: Vec<_> = resp.headers().get_all("content-length").iter().collect();
    assert_eq!(lengths, ["2"], "one content-length, the body's");
}

/// A `waitUntil` promise that rejects fails the work done after the answer,
/// as a panic in Rust's `after` work does: what it wrote rolls back and the
/// instance is replaced. The answer, already sent, and what the handler
/// wrote before answering stand.
#[tokio::test(flavor = "multi_thread")]
async fn review_wait_until_exception_rolls_back_partial_work() {
    let node = node("js_wait_until_rolls_back", 8);
    let r = post(&node, "u", "/later-throw").await;
    assert_eq!((r.status, r.body.as_str()), (200, "answered first"), "{:?}", r.error);
    // Served once the waitUntil work is over.
    let kv = get(&node, "u", "/kv").await.json();
    assert!(kv.get("partial").is_none(), "the failed work's write rolled back: {kv}");
    assert_eq!(kv["answered"], 1, "committed with the answer: {kv}");
    let r = get(&node, "u", "/").await;
    assert_eq!(r.json()["hits"], 2, "a fresh instance since: {}", r.body);
}

/// The component starts from a snapshot taken at build time; nothing in it
/// may be shared by instances or stale: randomness, the clock, the id.
#[tokio::test(flavor = "multi_thread")]
async fn instances_share_nothing_from_the_snapshot() {
    let node = node("js_snapshot_fresh", 8);
    let now_ms = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let (a, b) = (get(&node, "a", "/fresh").await.json(), get(&node, "b", "/fresh").await.json());
    assert_ne!(a["random"], b["random"], "Math.random is seeded per instance");
    assert_ne!(a["uuid"], b["uuid"]);
    assert!((a["now"].as_i64().unwrap() - now_ms()).abs() < 5000, "Date.now() is now: {a}");
    assert_eq!(get(&node, "b", "/").await.json()["object"], "b");
}

/// WebSocket events (as the proxy sends them) reach `webSocketOpen`,
/// `webSocketMessage` and `webSocketClose`; their answer carries the
/// socket's events, GRIP controls among them.
#[tokio::test(flavor = "multi_thread")]
async fn websocket_events() {
    let node = node("js_websocket_events", 8);
    let event = async |id: &str, body: &'static [u8]| {
        let req = http::Request::post("http://localhost/ws")
            .header("content-type", "application/websocket-events")
            .header("connection-id", id)
            .body(bytes::Bytes::from_static(body))
            .unwrap();
        let resp = node.dispatch("room", req).await;
        let status = resp.status().as_u16();
        let body = http_body_util::BodyExt::collect(resp.into_body()).await.unwrap().to_bytes();
        (status, body.to_vec())
    };
    let sub = r#"c:{"type":"subscribe","channel":"room"}"#;
    let open = format!("OPEN\r\nTEXT {:x}\r\n{sub}\r\n", sub.len()).into_bytes();
    assert_eq!(event("1", b"OPEN\r\n").await, (200, open));
    let echo = b"TEXT 10\r\nm:echo hi from 1\r\nBINARY 1\r\n\x07\r\n".to_vec();
    assert_eq!(event("1", b"TEXT 2\r\nhi\r\nBINARY 1\r\n\x07\r\n").await, (200, echo));
    assert_eq!(event("1", b"TEXT 3\r\nbye\r\n").await, (200, b"CLOSE 2\r\n\x0f\xa0\r\n".to_vec()), "close(4000)");
    assert_eq!(event("refuse-me", b"OPEN\r\n").await.0, 403);
    assert_eq!(event("1", b"CLOSE 2\r\n\x03\xe8\r\n").await, (200, vec![]));
    assert_eq!(get(&node, "room", "/kv").await.json(), serde_json::json!({"closed": 1000}), "webSocketClose ran");
}
