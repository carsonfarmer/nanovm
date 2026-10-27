//! End-to-end tests against the real guests in `guests/`. Build them first
//! with `scripts/build-guests.sh`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::BodyExt;
use nanovm::config::NodeConfig;
use nanovm::table::Node;
use serde_json::Value;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn guest(name: &str) -> PathBuf {
    let p = repo().join("guests/target/wasm32-wasip2/release").join(format!("{name}.wasm"));
    assert!(p.exists(), "{} is missing: run scripts/build-guests.sh", p.display());
    p
}

/// A node with the `counter` and `hostile` apps and its own data directory.
/// The compile cache is shared between tests.
fn node(test: &str, max_live: usize, counter_idle_ms: u64) -> Arc<Node> {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let dir = root.join("nodes").join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cache = root.join("cwasm-cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::os::unix::fs::symlink(&cache, dir.join("cache")).unwrap();

    let text = format!(
        r#"
        data_dir = "{dir}"
        max_live_objects = {max_live}
        memory_reservation_mb = 64

        [[app]]
        name = "counter"
        component = "{counter}"
        static_dir = "{statics}"
        [app.limits]
        memory_mb = 16
        cpu_ms = 500 # generous: oversubscribed CI hosts inflate the wall-time budget; CPU kills are tested on `hostile`
        request_timeout_ms = 2000
        idle_timeout_ms = {counter_idle_ms}
        data_mb = 1
        tmp_mb = 1
        max_files = 100
        max_file_mb = 1

        [[app]]
        name = "hostile"
        component = "{hostile}"
        static_dir = "{statics}"
        [app.limits]
        memory_mb = 32
        cpu_ms = 100
        request_timeout_ms = 1000
        post_return_timeout_ms = 500
        data_mb = 8
        tmp_mb = 4
        max_files = 200
        max_file_mb = 4
        max_resources = 2000
        max_outbound_concurrency = 2
        egress = ["example.com", "localhost"]
        "#,
        dir = dir.display(),
        counter = guest("counter").display(),
        hostile = guest("hostile").display(),
        statics = repo().join("demo/static").display(),
    );
    let cfg: NodeConfig = toml::from_str(&text).unwrap();
    cfg.validate().unwrap();
    Node::new(cfg).unwrap()
}

struct Resp {
    status: u16,
    /// `x-nanovm-error`: set when the host, not the guest, answered.
    error: Option<String>,
    body: String,
    elapsed: Duration,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }
}

async fn call(node: &Arc<Node>, method: &str, app: &str, id: &str, path: &str, body: &str) -> Resp {
    let req = http::Request::builder()
        .method(method)
        .uri(format!("http://localhost{path}"))
        .body(Bytes::from(body.to_string()))
        .unwrap();
    let t0 = Instant::now();
    let resp = node.dispatch(app, id, req).await;
    let status = resp.status().as_u16();
    let error = resp.headers().get("x-nanovm-error").map(|v| v.to_str().unwrap().to_string());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    Resp { status, error, body: String::from_utf8_lossy(&body).into(), elapsed: t0.elapsed() }
}

async fn get(node: &Arc<Node>, app: &str, id: &str, path: &str) -> Resp {
    call(node, "GET", app, id, path, "").await
}

/// Waits until `f` holds, for up to 2 s.
async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(2), "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Waits until every live object is idle, so LRU eviction has a candidate.
async fn settle(node: &Arc<Node>) {
    eventually("objects to go idle", || node.objects(usize::MAX).1.keys().all(|p| *p == "idle")).await;
}

// ---- persistence and isolation ----------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn volume_survives_eviction_memory_does_not() {
    let node = node("volume_survives_eviction", 2, 60_000);
    for n in 1..=3 {
        let r = call(&node, "POST", "counter", "a", "/incr", "").await;
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(r.json()["count"], n);
        assert_eq!(r.json()["instance_hits"], n);
    }
    // Two more objects on a two-slot node push `a` out.
    for id in ["b", "c"] {
        settle(&node).await;
        assert_eq!(get(&node, "counter", id, "/").await.status, 200);
    }
    assert!(node.metrics.evicted_lru.load(Relaxed) >= 1);
    settle(&node).await;
    let r = get(&node, "counter", "a", "/").await;
    assert_eq!(r.json()["count"], 3, "volume kept");
    assert_eq!(r.json()["instance_hits"], 1, "fresh instance");
}

#[tokio::test(flavor = "multi_thread")]
async fn objects_do_not_share_volumes() {
    let node = node("objects_do_not_share_volumes", 8, 60_000);
    assert_eq!(call(&node, "PUT", "counter", "x", "/kv/k", "secret").await.status, 204);
    let r = get(&node, "counter", "x", "/kv/k").await;
    assert_eq!((r.status, r.body.as_str()), (200, "secret"));
    let r = get(&node, "counter", "y", "/kv/k").await;
    assert_eq!(r.status, 404, "{}", r.body);
    assert_eq!(call(&node, "DELETE", "counter", "x", "/kv/k", "").await.status, 204);
    assert_eq!(get(&node, "counter", "x", "/kv/k").await.status, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn app_dir_is_readable() {
    let node = node("app_dir_is_readable", 8, 60_000);
    let r = get(&node, "counter", "s", "/static").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body, std::fs::read_to_string(repo().join("demo/static/hello.txt")).unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_routes() {
    let node = node("bad_routes", 8, 60_000);
    let r = get(&node, "nope", "a", "/").await;
    assert_eq!((r.status, r.error.as_deref()), (404, Some("no-such-app")));
    for id in ["", "..", ".hidden", "a/b", &"x".repeat(129)] {
        let r = get(&node, "counter", id, "/").await;
        assert_eq!((r.status, r.error.as_deref()), (400, Some("bad-object-id")), "{id:?}");
    }
}

// ---- concurrency model ------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn one_request_at_a_time_per_object() {
    let node = node("one_request_at_a_time", 8, 60_000);
    let reqs = (0..8).map(|_| {
        let node = node.clone();
        tokio::spawn(async move { get(&node, "hostile", "serial", "/concurrency?ms=20").await })
    });
    for r in futures::future::join_all(reqs).await {
        let r = r.unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.json()["max_active"], 1);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn objects_run_in_parallel() {
    let node = node("objects_run_in_parallel", 16, 60_000);
    let t0 = Instant::now();
    let reqs = (0..8).map(|i| {
        let node = node.clone();
        tokio::spawn(async move { get(&node, "hostile", &format!("p{i}"), "/sleep?ms=300").await })
    });
    for r in futures::future::join_all(reqs).await {
        assert_eq!(r.unwrap().status, 200);
    }
    assert!(t0.elapsed() < Duration::from_millis(900), "took {:?}", t0.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn full_queue_is_rejected() {
    let node = node("full_queue_is_rejected", 8, 60_000);
    let reqs = (0..80).map(|_| {
        let node = node.clone();
        tokio::spawn(async move { get(&node, "hostile", "q", "/sleep?ms=10").await })
    });
    let mut ok = 0;
    let mut busy = 0;
    for r in futures::future::join_all(reqs).await {
        match r.unwrap() {
            r if r.status == 200 => ok += 1,
            r if r.error.as_deref() == Some("object-busy") => busy += 1,
            r => panic!("{} {:?} {}", r.status, r.error, r.body),
        }
    }
    assert!(ok >= 64, "ok={ok}");
    assert!(busy >= 1, "busy={busy}");
    assert_eq!(node.metrics.rejected_busy.load(Relaxed), busy);
}

// ---- per-object limits ------------------------------------------------

/// Runs one attack, checks the host killed the instance with `reason`, and
/// that the object comes back on a fresh instance.
async fn killed(node: &Arc<Node>, id: &str, path: &str, status: u16, reason: &str) -> Resp {
    let before = get(node, "hostile", id, "/").await;
    assert_eq!(before.status, 200);
    let r = get(node, "hostile", id, path).await;
    assert_eq!((r.status, r.error.as_deref()), (status, Some(reason)), "{path}: {}", r.body);
    let after = get(node, "hostile", id, "/").await;
    assert_eq!(after.status, 200);
    assert_eq!(after.json()["hits"], 1, "{path}: instance was replaced");
    r
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_limit() {
    let node = node("memory_limit", 8, 60_000);
    let r = killed(&node, "mem", "/alloc?mb=1024", 500, "memory").await;
    assert!(r.body.contains("32MiB"), "{}", r.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn cpu_limit() {
    let node = node("cpu_limit", 8, 60_000);
    let r = killed(&node, "cpu", "/spin", 500, "cpu").await;
    // 100 ms budget: killed well before the 1 s wall-clock limit.
    assert!(r.elapsed < Duration::from_millis(800), "took {:?}", r.elapsed);
}

#[tokio::test(flavor = "multi_thread")]
async fn wall_clock_limit() {
    let node = node("wall_clock_limit", 8, 60_000);
    let r = killed(&node, "slow", "/sleep?ms=60000", 504, "timeout").await;
    assert!(r.elapsed >= Duration::from_millis(1000) && r.elapsed < Duration::from_millis(2500), "took {:?}", r.elapsed);
}

#[tokio::test(flavor = "multi_thread")]
async fn guest_trap() {
    let node = node("guest_trap", 8, 60_000);
    killed(&node, "boom", "/panic", 500, "trap").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resource_table_limit() {
    let node = node("resource_table_limit", 8, 60_000);
    killed(&node, "leak", "/leak", 500, "resource-table").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn kills_do_not_disturb_neighbours() {
    let node = node("kills_do_not_disturb_neighbours", 16, 60_000);
    let attacks: Vec<_> = ["/spin", "/alloc?mb=1024", "/panic", "/sleep?ms=60000", "/leak"]
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            let node = node.clone();
            tokio::spawn(async move { get(&node, "hostile", &format!("bad{i}"), p).await })
        })
        .collect();
    let t0 = Instant::now();
    let mut n = 0;
    while t0.elapsed() < Duration::from_millis(1200) {
        let r = get(&node, "counter", "canary", "/").await;
        assert_eq!(r.status, 200, "{}", r.body);
        n += 1;
    }
    for a in attacks {
        assert!(a.await.unwrap().status >= 500);
    }
    assert!(n > 100, "canary only got {n} requests through");
}

#[tokio::test(flavor = "multi_thread")]
async fn cpu_hogs_leave_cores_for_short_requests() {
    let node = node("cpu_hogs_leave_cores_for_short_requests", 128, 60_000);
    let cores = std::thread::available_parallelism().unwrap().get();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Four spinners per core: without heavy lanes they hold every worker.
    let hogs: Vec<_> = (0..4 * cores)
        .map(|i| {
            let (node, stop) = (node.clone(), stop.clone());
            tokio::spawn(async move {
                while !stop.load(Relaxed) {
                    assert_eq!(get(&node, "hostile", &format!("hog{i}"), "/spin").await.error.as_deref(), Some("cpu"));
                }
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut lat = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(1000) {
        let r = get(&node, "counter", "canary", "/").await;
        assert_eq!(r.status, 200, "{}", r.body);
        lat.push(r.elapsed);
    }
    stop.store(true, Relaxed);
    for h in hogs {
        h.await.unwrap();
    }
    lat.sort();
    let p99 = lat[lat.len() * 99 / 100];
    assert!(lat.len() > 200, "canary only got {} requests through", lat.len());
    assert!(p99 < Duration::from_millis(50), "canary p99 {p99:?} with {} hogs", 4 * cores);
}

#[tokio::test(flavor = "multi_thread")]
async fn grid_marks_killed_objects() {
    let node = node("grid_marks_killed_objects", 8, 60_000);
    assert_eq!(get(&node, "hostile", "g", "/panic").await.status, 500);
    eventually("an X cell", || node.grid().contains('X')).await;
}

// ---- filesystem layout and quotas -------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn file_size_limit() {
    let node = node("file_size_limit", 8, 60_000);
    let r = get(&node, "hostile", "f", "/fill?mount=data&mb=100").await;
    assert_eq!(r.status, 507);
    assert_eq!(r.json()["written"], 4 << 20);
    assert_eq!(r.json()["error"], "file-too-large");
}

#[tokio::test(flavor = "multi_thread")]
async fn file_count_limit() {
    let node = node("file_count_limit", 8, 60_000);
    let r = get(&node, "hostile", "f", "/files?mount=tmp").await;
    assert_eq!(r.status, 507);
    assert_eq!(r.json()["created"], 200);
    assert_eq!(r.json()["error"], "quota");
}

#[tokio::test(flavor = "multi_thread")]
async fn tmp_quota_and_wipe() {
    let node = node("tmp_quota_and_wipe", 8, 60_000);
    assert_eq!(get(&node, "hostile", "t", "/fill?mount=tmp&mb=4&name=a").await.status, 200);
    let r = get(&node, "hostile", "t", "/fill?mount=tmp&mb=1&name=b").await;
    assert_eq!(r.status, 507);
    assert_eq!(r.json()["error"], "insufficient-space");
    // A new instance gets an empty /tmp.
    assert_eq!(get(&node, "hostile", "t", "/panic").await.status, 500);
    assert_eq!(get(&node, "hostile", "t", "/fill?mount=tmp&mb=4&name=a").await.status, 200);
    // and the old one's scratch directory is gone from the host.
    let scratch = node.cfg.data_dir.join("tmp/hostile");
    eventually("old scratch dir removed", || {
        std::fs::read_dir(&scratch).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with("t.")).count() == 1
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_quota_accounting() {
    let node = node("rename_quota_accounting", 8, 60_000);
    assert_eq!(get(&node, "hostile", "r", "/fill?mount=tmp&mb=4&name=a").await.status, 200);
    // Renaming a file onto itself is a no-op and must not credit the quota.
    let r = get(&node, "hostile", "r", "/rename?mount=tmp&from=a&n=10").await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = get(&node, "hostile", "r", "/fill?mount=tmp&mb=1&name=b").await;
    assert_eq!(r.status, 507, "{}", r.body);
    assert_eq!(r.json()["error"], "insufficient-space");
    // Renaming the empty `b` over `a` does free a's 4 MiB.
    let r = get(&node, "hostile", "r", "/rename?mount=tmp&from=b&to=a").await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = get(&node, "hostile", "r", "/fill?mount=tmp&mb=4&name=c").await;
    assert_eq!(r.status, 200, "{}", r.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn data_quota_survives_restart() {
    let node = node("data_quota_survives_restart", 8, 60_000);
    for name in ["a", "b"] {
        let r = get(&node, "hostile", "d", &format!("/fill?mount=data&mb=4&name={name}")).await;
        assert_eq!(r.status, 200, "{}", r.body);
    }
    assert_eq!(get(&node, "hostile", "d", "/panic").await.status, 500);
    // The new instance rescans the volume: it is still full.
    let r = get(&node, "hostile", "d", "/fill?mount=data&mb=1&name=c").await;
    assert_eq!(r.status, 507, "{}", r.body);
    assert_eq!(r.json()["error"], "insufficient-space");
    let vol = node.cfg.data_dir.join("objects/hostile/d");
    assert_eq!(std::fs::metadata(vol.join("a")).unwrap().len(), 4 << 20);
    assert!(node.metrics.fs_denied.load(Relaxed) >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn app_dir_is_read_only() {
    let node = node("app_dir_is_read_only", 8, 60_000);
    let r = get(&node, "hostile", "w", "/write-app").await;
    assert_eq!(r.status, 507);
    assert_eq!(r.json()["error"], "not-permitted");
    assert!(!repo().join("demo/static/pwned.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_escapes_blocked() {
    let node = node("sandbox_escapes_blocked", 8, 60_000);
    let r = get(&node, "hostile", "e", "/escape").await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(!r.body.contains("LEAKED") && !r.body.contains("OPENED"), "{}", r.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_rename_across_mounts() {
    let node = node("no_rename_across_mounts", 8, 60_000);
    let r = get(&node, "hostile", "r", "/cross-rename").await;
    assert_eq!(r.status, 507, "{}", r.body);
    assert_eq!(r.json()["error"], "cross-device");
}

// ---- egress -----------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn egress_unlisted_host_denied() {
    let node = node("egress_unlisted_host_denied", 8, 60_000);
    let r = get(&node, "hostile", "n", "/fetch?url=http://169.254.169.254/latest/meta-data/").await;
    assert_eq!(r.status, 502);
    assert!(r.json()["denied"].as_str().unwrap().contains("HttpRequestDenied"), "{}", r.body);
    assert!(node.metrics.egress_denied.load(Relaxed) >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn egress_listed_name_resolving_to_loopback_denied() {
    let node = node("egress_loopback_denied", 8, 60_000);
    // Something is listening, so only the IP filter can stop the request.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let r = get(&node, "hostile", "n", &format!("/fetch?url=http://localhost:{port}/")).await;
    assert_eq!(r.status, 502);
    assert!(r.json()["denied"].as_str().unwrap().contains("DestinationIpProhibited"), "{}", r.body);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the internet"]
async fn egress_listed_host_allowed() {
    let node = node("egress_listed_host_allowed", 8, 60_000);
    let r = get(&node, "hostile", "n", "/fetch?url=https://example.com/").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["status"], 200);
}

// ---- eviction and capacity --------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn lru_eviction_keeps_node_under_capacity() {
    let node = node("lru_eviction", 4, 60_000);
    for i in 0..10 {
        settle(&node).await;
        let r = get(&node, "counter", &format!("o{i}"), "/").await;
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(node.live_objects() <= 4);
    }
    assert!(node.metrics.evicted_lru.load(Relaxed) >= 6);
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_objects_are_evicted() {
    let node = node("idle_objects_are_evicted", 8, 100);
    assert_eq!(get(&node, "counter", "i", "/").await.status, 200);
    eventually("idle eviction", || node.live_objects() == 0).await;
    assert_eq!(node.metrics.evicted_idle.load(Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn at_capacity_with_no_idle_object() {
    let node = node("at_capacity_with_no_idle_object", 2, 60_000);
    let busy: Vec<_> = (0..2)
        .map(|i| {
            let node = node.clone();
            tokio::spawn(async move { get(&node, "hostile", &format!("b{i}"), "/sleep?ms=500").await })
        })
        .collect();
    eventually("both objects busy", || node.objects(8).1.get("handling") == Some(&2)).await;
    let r = get(&node, "counter", "late", "/").await;
    assert_eq!((r.status, r.error.as_deref()), (503, Some("node-at-capacity")));
    for b in busy {
        assert_eq!(b.await.unwrap().status, 200);
    }
    settle(&node).await;
    assert_eq!(get(&node, "counter", "late", "/").await.status, 200);
}

/// Regression: an evicted instance frees its pool slot only after its actor
/// exits, so churning through new objects used to fail instantiation. Now a
/// new store waits for a slot, so live stores never pass `max_live_objects`.
#[tokio::test(flavor = "multi_thread")]
async fn churn_past_capacity_never_fails() {
    let node = node("churn_past_capacity", 16, 60_000);
    let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let workers = (0..8).map(|_| {
        let (node, next) = (node.clone(), next.clone());
        tokio::spawn(async move {
            loop {
                let i = next.fetch_add(1, Relaxed);
                if i >= 3000 {
                    return;
                }
                let r = get(&node, "counter", &format!("churn{i}"), "/").await;
                assert_eq!(r.status, 200, "object {i}: {:?} {}", r.error, r.body);
            }
        })
    });
    for w in futures::future::join_all(workers).await {
        w.unwrap();
    }
    let peak = node.metrics.stores_peak.load(Relaxed);
    assert!(peak <= 16, "{peak} live stores");
    assert!(node.live_objects() <= 16);
    assert!(node.metrics.evicted_lru.load(Relaxed) >= 3000 - 16);
}

// ---- HTTP front end ---------------------------------------------------

/// Sends one raw HTTP/1.1 request and returns the status code and body.
async fn raw(addr: std::net::SocketAddr, head: &str, body: &[u8]) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(head.as_bytes()).await.unwrap();
    // The server may answer and close before reading an oversized body.
    let _ = s.write_all(body).await;
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out).await;
    let text = String::from_utf8_lossy(&out).to_string();
    let status = text.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    (status, body)
}

#[tokio::test(flavor = "multi_thread")]
async fn http_front_end() {
    let node = node("http_front_end", 8, 60_000);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(nanovm::server::serve(node.clone(), listener));

    let (status, body) = raw(addr, "GET /counter/web/ HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n", b"").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""object":"web""#), "{body}");

    let (status, _) = raw(addr, "GET /counter HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n", b"").await;
    assert_eq!(status, 404);

    let big = vec![b'x'; 2 << 20];
    let head = format!("POST /counter/web/kv/big HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", big.len());
    let (status, _) = raw(addr, &head, &big).await;
    assert_eq!(status, 413);

    let (status, body) = raw(addr, "GET /_nanovm/stats HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n", b"").await;
    assert_eq!(status, 200);
    assert!(body.contains("\"live_objects\""), "{body}");

    let (status, _) = raw(addr, "POST /_nanovm/demo/spawn HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", b"").await;
    assert_eq!(status, 404, "demo endpoints are off unless enabled");
}
