//! End-to-end tests against the real guests in `guests/`. Build them first
//! with `scripts/build-guests.sh`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use nanovm::config::{AppConfig, NodeConfig};
#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;

use load::{Reply, request};
use nanovm::table::Node;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn guest(name: &str) -> Vec<u8> {
    let p = repo()
        .join("guests/target/wasm32-wasip2/release")
        .join(format!("{name}.wasm"));
    std::fs::read(&p).unwrap_or_else(|_| panic!("{} is missing: run scripts/build-guests.sh", p.display()))
}

/// The test apps, `counter` and `hostile`, as their registry configs give
/// them.
fn app(name: &str) -> AppConfig {
    let limits = match name {
        // A generous CPU budget: oversubscribed CI hosts inflate the
        // wall-time budget; CPU kills are tested on `hostile`.
        "counter" => serde_json::json!({
            "memory_mb": 16, "cpu_ms": 500, "request_timeout_ms": 2000,
            "idle_timeout_ms": 60000, "disk_mb": 1, "max_files": 100,
        }),
        "hostile" => serde_json::json!({
            "memory_mb": 32, "cpu_ms": 100, "request_timeout_ms": 1000,
            "post_return_timeout_ms": 500, "disk_mb": 8, "max_files": 200,
            "max_resources": 2000, "max_outbound_concurrency": 2,
            "egress": ["example.com", "localhost"],
        }),
        _ => panic!("no test app {name}"),
    };
    AppConfig {
        name: name.into(),
        env: Default::default(),
        config: Default::default(),
        limits: serde_json::from_value(limits).unwrap(),
        worker: Default::default(),
    }
}

/// A dev-mode node config with its own data directory; the compile cache is
/// shared between tests.
fn node_cfg(test: &str, max_live: usize) -> NodeConfig {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let dir = root.join("nodes").join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cache = root.join("cwasm-cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::os::unix::fs::symlink(&cache, dir.join("cache")).unwrap();
    let cfg = NodeConfig {
        data_dir: dir,
        max_live_objects: max_live,
        memory_reservation_mb: 64,
        dev: true,
        ..NodeConfig::default()
    };
    cfg.validate().unwrap();
    cfg
}

/// An in-process node running `app` (named for its guest).
fn node_with(cfg: NodeConfig, app: AppConfig) -> Arc<Node> {
    nanovm::engine::preflight(&cfg).unwrap();
    Node::new(cfg, &app, &guest(&app.name), None).unwrap()
}

/// An in-process node running the test app `name`.
fn node(test: &str, name: &str, max_live: usize) -> Arc<Node> {
    node_with(node_cfg(test, max_live), app(name))
}

async fn get(node: &Arc<Node>, id: &str, path: &str) -> Reply {
    request(node, "GET", id, path, "").await
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
    eventually("objects to go idle", || node.phases().keys().all(|p| *p == "idle")).await;
}

// ---- persistence and isolation ----------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn volume_survives_eviction_memory_does_not() {
    let node = node("volume_survives_eviction", "counter", 2);
    for n in 1..=3 {
        let r = request(&node, "POST", "a", "/incr", "").await;
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(r.json()["count"], n);
        assert_eq!(r.json()["instance_hits"], n);
    }
    // Two more objects on a two-slot node push `a` out.
    for id in ["b", "c"] {
        settle(&node).await;
        assert_eq!(get(&node, id, "/").await.status, 200);
    }
    assert!(node.metrics.evicted_lru.load(Relaxed) >= 1);
    settle(&node).await;
    let r = get(&node, "a", "/").await;
    assert_eq!(r.json()["count"], 3, "volume kept");
    assert_eq!(r.json()["instance_hits"], 1, "fresh instance");
}

#[tokio::test(flavor = "multi_thread")]
async fn objects_do_not_share_volumes() {
    let node = node("objects_do_not_share_volumes", "counter", 8);
    assert_eq!(
        request(&node, "PUT", "x", "/kv/k", "secret").await.status,
        204
    );
    let r = get(&node, "x", "/kv/k").await;
    assert_eq!((r.status, r.body.as_str()), (200, "secret"));
    let r = get(&node, "y", "/kv/k").await;
    assert_eq!(r.status, 404, "{}", r.body);
    assert_eq!(request(&node, "DELETE", "x", "/kv/k", "").await.status, 204);
    assert_eq!(get(&node, "x", "/kv/k").await.status, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_routes() {
    let node = node("bad_routes", "counter", 8);
    for id in ["", "..", ".hidden", "a/b", "_nanovm", &"x".repeat(129)] {
        let r = get(&node, id, "/").await;
        assert_eq!((r.status, r.error.as_deref()), (400, Some("bad-object-id")), "{id:?}");
    }
}

// ---- concurrency model ------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn one_request_at_a_time_per_object() {
    let node = node("one_request_at_a_time", "hostile", 8);
    let reqs = (0..8).map(|_| {
        let node = node.clone();
        tokio::spawn(async move { get(&node, "serial", "/concurrency?ms=20").await })
    });
    for r in futures::future::join_all(reqs).await {
        let r = r.unwrap();
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(r.json()["max_active"], 1);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn objects_run_in_parallel() {
    let node = node("objects_run_in_parallel", "hostile", 16);
    let t0 = Instant::now();
    let reqs = (0..8).map(|i| {
        let node = node.clone();
        tokio::spawn(async move { get(&node, &format!("p{i}"), "/sleep?ms=300").await })
    });
    for r in futures::future::join_all(reqs).await {
        assert_eq!(r.unwrap().status, 200);
    }
    assert!(t0.elapsed() < Duration::from_millis(900), "took {:?}", t0.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn full_queue_is_rejected() {
    let node = node("full_queue_is_rejected", "hostile", 8);
    let reqs = (0..80).map(|_| {
        let node = node.clone();
        tokio::spawn(async move { get(&node, "q", "/sleep?ms=10").await })
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
async fn killed(node: &Arc<Node>, id: &str, path: &str, status: u16, reason: &str) -> Reply {
    let before = get(node, id, "/").await;
    assert_eq!(before.status, 200, "{:?} {}", before.error, before.body);
    let r = get(node, id, path).await;
    assert_eq!(
        (r.status, r.error.as_deref()),
        (status, Some(reason)),
        "{path}: {}",
        r.body
    );
    let after = get(node, id, "/").await;
    assert_eq!(after.status, 200, "{:?} {}", after.error, after.body);
    assert_eq!(after.json()["hits"], 1, "{path}: instance was replaced");
    r
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_limit() {
    let node = node("memory_limit", "hostile", 8);
    let r = killed(&node, "mem", "/alloc?mb=1024", 500, "memory").await;
    assert!(r.body.contains("32MiB"), "{}", r.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn cpu_limit() {
    let node = node("cpu_limit", "hostile", 8);
    let r = killed(&node, "cpu", "/spin", 500, "cpu").await;
    // 100 ms budget: killed well before the 1 s wall-clock limit.
    assert!(r.elapsed < Duration::from_millis(800), "took {:?}", r.elapsed);
}

/// Regression: CPU charged to the killed request used to carry over into the
/// next instantiation, so an epoch tick during it killed the new instance
/// at birth (503 instantiate-failed). A 1 ms tick makes that tick likely.
#[tokio::test(flavor = "multi_thread")]
async fn cpu_kill_does_not_poison_next_instance() {
    let mut cfg = node_cfg("cpu_kill_does_not_poison_next_instance", 8);
    cfg.epoch_tick_ms = 1;
    let node = node_with(cfg, app("hostile"));
    for i in 0..60 {
        let r = get(&node, "k", "/spin").await;
        assert_eq!(r.error.as_deref(), Some("cpu"), "{}", r.body);
        let r = get(&node, "k", "/").await;
        assert_eq!(r.status, 200, "round {i}: {:?} {}", r.error, r.body);
    }
}

/// Regression: instantiation was charged wall time since its slot was
/// acquired, including host work before the guest ran (scanning `/data`),
/// so an epoch tick during start-up could kill a new instance that had
/// used no CPU. Here the scan alone outlasts the 10 ms budget.
#[tokio::test(flavor = "multi_thread")]
async fn start_up_is_charged_guest_cpu_not_host_work() {
    let mut cfg = node_cfg("start_up_is_charged_guest_cpu", 8);
    cfg.epoch_tick_ms = 1;
    let vol = cfg.data_dir.join("objects/hostile/s");
    std::fs::create_dir_all(&vol).unwrap();
    for i in 0..20_000 {
        std::fs::write(vol.join(format!("f{i}")), b"").unwrap();
    }
    let mut hostile = app("hostile");
    hostile.limits.cpu_ms = 10;
    let node = node_with(cfg, hostile);
    for i in 0..30 {
        let r = get(&node, "s", "/").await;
        assert_eq!(r.status, 200, "round {i}: {:?} {}", r.error, r.body);
        assert_eq!(get(&node, "s", "/panic").await.status, 500);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn wall_clock_limit() {
    let node = node("wall_clock_limit", "hostile", 8);
    let r = killed(&node, "slow", "/sleep?ms=60000", 504, "timeout").await;
    assert!(
        r.elapsed >= Duration::from_millis(1000) && r.elapsed < Duration::from_millis(2500),
        "took {:?}",
        r.elapsed
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn guest_trap() {
    let node = node("guest_trap", "hostile", 8);
    killed(&node, "boom", "/panic", 500, "trap").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resource_table_limit() {
    let node = node("resource_table_limit", "hostile", 8);
    killed(&node, "leak", "/leak", 500, "resource-table").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn kills_do_not_disturb_neighbours() {
    let node = node("kills_do_not_disturb_neighbours", "hostile", 16);
    let attacks: Vec<_> = ["/spin", "/alloc?mb=1024", "/panic", "/sleep?ms=60000", "/leak"]
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            let node = node.clone();
            tokio::spawn(async move { get(&node, &format!("bad{i}"), p).await })
        })
        .collect();
    let t0 = Instant::now();
    let mut n = 0;
    while t0.elapsed() < Duration::from_millis(1200) {
        let r = get(&node, "canary", "/").await;
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
    let node = node("cpu_hogs_leave_cores_for_short_requests", "hostile", 128);
    let cores = std::thread::available_parallelism().unwrap().get();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Four spinners per core: without heavy lanes they hold every worker.
    let hogs: Vec<_> = (0..4 * cores)
        .map(|i| {
            let (node, stop) = (node.clone(), stop.clone());
            tokio::spawn(async move {
                while !stop.load(Relaxed) {
                    let r = get(&node, &format!("hog{i}"), "/spin").await;
                    assert_eq!(r.error.as_deref(), Some("cpu"), "{}", r.body);
                }
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut lat = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(1000) {
        let r = get(&node, "canary", "/").await;
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
    assert!(
        p99 < Duration::from_millis(50),
        "canary p99 {p99:?} with {} hogs",
        4 * cores
    );
}

// ---- filesystem layout and quotas -------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn file_count_limit() {
    // 200 creates run inline and count against the CPU budget, which on a
    // loaded small host can pass 100 ms; this test is about the file count.
    let mut hostile = app("hostile");
    hostile.limits.cpu_ms = 1_000;
    let node = node_with(node_cfg("file_count_limit", 8), hostile);
    let r = get(&node, "f", "/files?mount=tmp").await;
    assert_eq!(r.status, 507, "{}", r.body);
    assert_eq!(r.json()["created"], 200);
    assert_eq!(r.json()["error"], "quota");
}

#[tokio::test(flavor = "multi_thread")]
async fn disk_quota_and_tmp_wipe() {
    let node = node("disk_quota_and_tmp_wipe", "hostile", 8);
    let fill = async |name: &str, mount: &str, mb: u32| {
        get(
            &node,
            "t",
            &format!("/fill?mount={mount}&mb={mb}&name={name}"),
        )
        .await
    };
    // /tmp and /data share one 8 MiB budget.
    assert_eq!(fill("a", "tmp", 4).await.status, 200);
    assert_eq!(fill("b", "data", 4).await.status, 200);
    let r = fill("c", "tmp", 1).await;
    assert_eq!(r.status, 507, "{}", r.body);
    assert_eq!(r.json()["error"], "insufficient-space");
    // A new instance gets an empty /tmp, which frees its share.
    assert_eq!(get(&node, "t", "/panic").await.status, 500);
    assert_eq!(fill("a", "tmp", 4).await.status, 200);
    // and the old one's scratch directory is gone from the host.
    let scratch = node.cfg.data_dir.join("tmp/hostile");
    eventually("old scratch dir removed", || {
        std::fs::read_dir(&scratch)
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with("t."))
            .count()
            == 1
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_quota_accounting() {
    let node = node("rename_quota_accounting", "hostile", 8);
    for name in ["a", "b"] {
        let r = get(&node, "r", &format!("/fill?mount=tmp&mb=4&name={name}")).await;
        assert_eq!(r.status, 200, "{}", r.body);
    }
    // Renaming a file onto itself is a no-op and must not credit the quota.
    let r = get(&node, "r", "/rename?mount=tmp&from=a&n=10").await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = get(&node, "r", "/fill?mount=tmp&mb=1&name=c").await;
    assert_eq!(r.status, 507, "{}", r.body);
    assert_eq!(r.json()["error"], "insufficient-space");
    // Renaming `c` (empty, from the refused fill) over `a` frees a's 4 MiB.
    let r = get(&node, "r", "/rename?mount=tmp&from=c&to=a").await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = get(&node, "r", "/fill?mount=tmp&mb=4&name=d").await;
    assert_eq!(r.status, 200, "{}", r.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn data_quota_survives_restart() {
    let node = node("data_quota_survives_restart", "hostile", 8);
    for name in ["a", "b"] {
        let r = get(&node, "d", &format!("/fill?mount=data&mb=4&name={name}")).await;
        assert_eq!(r.status, 200, "{}", r.body);
    }
    assert_eq!(get(&node, "d", "/panic").await.status, 500);
    // The new instance rescans the volume: it is still full.
    let r = get(&node, "d", "/fill?mount=data&mb=1&name=c").await;
    assert_eq!(r.status, 507, "{}", r.body);
    assert_eq!(r.json()["error"], "insufficient-space");
    let vol = node.cfg.data_dir.join("objects/hostile/d");
    assert_eq!(std::fs::metadata(vol.join("a")).unwrap().len(), 4 << 20);
    assert!(node.metrics.fs_denied.load(Relaxed) >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_escapes_blocked() {
    let node = node("sandbox_escapes_blocked", "hostile", 8);
    let r = get(&node, "e", "/escape").await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(!r.body.contains("LEAKED") && !r.body.contains("OPENED"), "{}", r.body);
}

/// `/data` and `/tmp` are separate filesystems to the guest: a rename
/// between them is refused as cross-device, as POSIX does.
#[tokio::test(flavor = "multi_thread")]
async fn renames_between_mounts_are_cross_device() {
    let node = node("renames_between_mounts_are_cross_device", "hostile", 8);
    let r = get(&node, "r", "/cross-rename").await;
    assert_eq!(r.json()["error"], "cross-device", "{}", r.body);
    assert!(!node.cfg.data_dir.join("objects/hostile/r/x.txt").exists());
}

// ---- egress -----------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn egress_unlisted_host_denied() {
    let node = node("egress_unlisted_host_denied", "hostile", 8);
    let r = get(
        &node,
        "n",
        "/fetch?url=http://169.254.169.254/latest/meta-data/",
    )
    .await;
    assert_eq!(r.status, 502, "{}", r.body);
    assert!(
        r.json()["denied"].as_str().unwrap().contains("HttpRequestDenied"),
        "{}",
        r.body
    );
    assert!(node.metrics.egress_denied.load(Relaxed) >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn egress_allowed_request_round_trips() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // In dev mode an IP-literal allowlist entry may be private; this
    // exercises the whole send path.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let n = s.read(&mut buf).await.unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("GET /hello HTTP/1.1"));
        s.write_all(b"HTTP/1.1 201 Created\r\ncontent-length: 5\r\n\r\nhello")
            .await
            .unwrap();
    });
    let mut hostile = app("hostile");
    hostile.limits.egress.push(format!("127.0.0.1:{port}"));
    let node = node_with(node_cfg("egress_allowed_request_round_trips", 8), hostile);
    let r = get(
        &node,
        "n",
        &format!("/fetch?url=http://127.0.0.1:{port}/hello"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        (r.json()["status"].as_u64(), r.json()["body"].as_str()),
        (Some(201), Some("hello"))
    );
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn egress_listed_name_resolving_to_loopback_denied() {
    let node = node("egress_loopback_denied", "hostile", 8);
    // Something is listening, so only the IP filter can stop the request.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let r = get(&node, "n", &format!("/fetch?url=http://localhost:{port}/")).await;
    assert_eq!(r.status, 502, "{}", r.body);
    assert!(
        r.json()["denied"].as_str().unwrap().contains("DestinationIpProhibited"),
        "{}",
        r.body
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the internet"]
async fn egress_listed_host_allowed() {
    let node = node("egress_listed_host_allowed", "hostile", 8);
    let r = get(&node, "n", "/fetch?url=https://example.com/").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["status"], 200);
}

// ---- eviction and capacity --------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn lru_eviction_keeps_node_under_capacity() {
    let node = node("lru_eviction", "counter", 4);
    for i in 0..10 {
        settle(&node).await;
        let r = get(&node, &format!("o{i}"), "/").await;
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(node.live_objects() <= 4);
    }
    assert!(node.metrics.evicted_lru.load(Relaxed) >= 6);
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_objects_are_evicted() {
    let mut counter = app("counter");
    counter.limits.idle_timeout_ms = 100;
    let node = node_with(node_cfg("idle_objects_are_evicted", 8), counter);
    assert_eq!(get(&node, "i", "/").await.status, 200);
    eventually("idle eviction", || node.live_objects() == 0).await;
    assert_eq!(node.metrics.evicted_idle.load(Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn at_capacity_with_no_idle_object() {
    let node = node("at_capacity_with_no_idle_object", "hostile", 2);
    let busy: Vec<_> = (0..2)
        .map(|i| {
            let node = node.clone();
            tokio::spawn(async move { get(&node, &format!("b{i}"), "/sleep?ms=500").await })
        })
        .collect();
    eventually("both objects busy", || node.phases().get("handling") == Some(&2)).await;
    let r = get(&node, "late", "/").await;
    assert_eq!((r.status, r.error.as_deref()), (503, Some("node-at-capacity")));
    for b in busy {
        assert_eq!(b.await.unwrap().status, 200);
    }
    settle(&node).await;
    assert_eq!(get(&node, "late", "/").await.status, 200);
}

/// Regression: an evicted instance frees its pool slot only after its actor
/// exits, so churning through new objects used to fail instantiation. Now a
/// new store waits for a slot, so live stores never pass `max_live_objects`.
#[tokio::test(flavor = "multi_thread")]
async fn churn_past_capacity_never_fails() {
    let node = node("churn_past_capacity", "counter", 16);
    let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let workers = (0..8).map(|_| {
        let (node, next) = (node.clone(), next.clone());
        tokio::spawn(async move {
            loop {
                let i = next.fetch_add(1, Relaxed);
                if i >= 3000 {
                    return;
                }
                let r = get(&node, &format!("churn{i}"), "/").await;
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
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

#[tokio::test(flavor = "multi_thread")]
async fn http_front_end() {
    let node = node("http_front_end", "counter", 8);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let n = node.clone();
    tokio::spawn(nanovm::server::serve_http1(listener, move |req| nanovm::server::handle(n.clone(), req)));

    let (status, body) = raw(
        addr,
        "GET /web/ HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        b"",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""object":"web""#), "{body}");

    let (status, _) = raw(
        addr,
        "GET //web/ HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        b"",
    )
    .await;
    assert_eq!(status, 400);

    let big = vec![b'x'; 2 << 20];
    let head = format!(
        "POST /web/kv/big HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        big.len()
    );
    let (status, _) = raw(addr, &head, &big).await;
    assert_eq!(status, 413);

    let (status, body) = raw(
        addr,
        "GET /_nanovm/stats HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        b"",
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains("\"live_objects\""), "{body}");

    let (status, _) = raw(
        addr,
        "GET /_nanovm/nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        b"",
    )
    .await;
    assert_eq!(status, 404);
}

/// A supervisor (one jailed worker per app, in dev mode) behind an HTTP
/// listener.
async fn supervised(test: &str) -> (Arc<nanovm::supervisor::Supervisor>, std::net::SocketAddr) {
    supervised_with(node_cfg(test, 8)).await
}

async fn supervised_with(cfg: NodeConfig) -> (Arc<nanovm::supervisor::Supervisor>, std::net::SocketAddr) {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_nanovm"));
    let sup = nanovm::supervisor::Supervisor::new(cfg, exe, 1).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let s = sup.clone();
    tokio::spawn(nanovm::server::serve_http1(listener, move |req| s.clone().handle(req)));
    (sup, addr)
}

/// A registry on loopback for the whole test run, as dev mode allows.
fn registry() -> std::net::SocketAddr {
    static REGISTRY: std::sync::OnceLock<container_registry::test_support::RunningRegistry> =
        std::sync::OnceLock::new();
    REGISTRY
        .get_or_init(|| container_registry::ContainerRegistry::builder().build_for_testing().run_in_background())
        .bound_addr()
}

/// Publishes `app` (its guest is the one its name says) to the test registry
/// as `nanovm/<repo>`, and returns it by digest, as `nanovm-app` names it.
async fn publish(repo: &str, app: &AppConfig) -> String {
    use nanovm::registry::{APP_TYPE, CONFIG_TYPE, VERSION, WASM_TYPE};
    use oci_client::client::{ClientConfig, ClientProtocol, Config, ImageLayer};
    let layer = ImageLayer::new(guest(&app.name), WASM_TYPE.into(), None);
    let spec = serde_json::json!({ "limits": app.limits, "env": app.env, "config": app.config });
    let config = Config::new(serde_json::to_vec(&spec).unwrap(), CONFIG_TYPE.into(), None);
    let mut manifest = oci_client::manifest::OciImageManifest::build(
        std::slice::from_ref(&layer),
        &config,
        Some([(VERSION.to_string(), "1.0.0".to_string())].into()),
    );
    manifest.artifact_type = Some(APP_TYPE.into());
    manifest.media_type = Some(oci_client::manifest::OCI_IMAGE_MEDIA_TYPE.into());
    let r: oci_client::Reference = format!("{}/nanovm/{repo}:latest", registry()).parse().unwrap();
    let client = oci_client::Client::new(ClientConfig {
        protocol: ClientProtocol::Http,
        ..Default::default()
    });
    let auth = oci_client::secrets::RegistryAuth::Anonymous;
    client.push(&r, &[layer], config, &auth, Some(manifest)).await.unwrap();
    let digest = nanovm::registry::Oci::new(true).unwrap().digest(&r).await.unwrap();
    format!("{}/nanovm/{repo}@{digest}", registry())
}

/// The headers naming `app` to a dev-mode node, which checks no signature.
fn names(app: &str) -> String {
    format!("nanovm-app: {app}\r\nnanovm-trust: dev dev\r\n")
}

/// The app's name on the node (its worker's, in stats and cgroups).
fn worker(app: &str) -> String {
    nanovm::registry::app_name("", &app.parse().unwrap())
}

/// `GET path` on `app`.
async fn app_get(addr: std::net::SocketAddr, app: &str, path: &str) -> (u16, String) {
    let head = format!("GET {path} HTTP/1.1\r\nHost: x\r\n{}Connection: close\r\n\r\n", names(app));
    raw(addr, &head, b"").await
}

async fn http_get(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
    raw(addr, &format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"), b"").await
}

async fn stats_json(addr: std::net::SocketAddr) -> serde_json::Value {
    let (status, body) = http_get(addr, "/_nanovm/stats").await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn supervised_workers_contain_attacks() {
    let (_sup, addr) = supervised("supervised_workers_contain_attacks").await;
    let (counter, hostile) = (publish("counter", &app("counter")).await, publish("hostile", &app("hostile")).await);

    let (status, body) = app_get(addr, &counter, "/s/").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""object":"s""#), "{body}");

    for (path, error) in [
        ("/s/alloc?mb=1024", r#""error":"memory""#),
        ("/s/spin", r#""error":"cpu""#),
        ("/s/panic", r#""error":"trap""#),
        // The supervisor applies the allowlist and the IP filter.
        ("/s/fetch?url=http://169.254.169.254/", "HttpRequestDenied"),
        ("/s/fetch?url=http://localhost:9/", "DestinationIpProhibited"),
        ("/s/escape", "not-permitted"),
    ] {
        let (_, body) = app_get(addr, &hostile, path).await;
        assert!(body.contains(error), "{path}: {body}");
    }
    // The worker survived all of it.
    let (status, _) = app_get(addr, &hostile, "/s/").await;
    assert_eq!(status, 200);
    // A request must name its app, and its object, strictly.
    let (status, body) = http_get(addr, "/s/").await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(app_get(addr, &hostile, "//s/").await.0, 400);
    assert_eq!(app_get(addr, &hostile, "/_nanovm/stats").await.0, 400);

    let v = stats_json(addr).await;
    assert_eq!(v["apps"][worker(&hostile).as_str()]["egress_denied"], 2, "{v}");
    for app in [worker(&counter), worker(&hostile)] {
        let pid = v["workers"][&app]["pid"].as_u64().unwrap();
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let field = |k: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(k))
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .to_string()
        };
        assert_eq!(field("Seccomp:"), "2", "{app}");
        assert_eq!(field("NoNewPrivs:"), "1", "{app}");
        if unsafe { libc::geteuid() } == 0 {
            let uid: u32 = field("Uid:").parse().unwrap();
            assert!(uid >= 200_000, "{app} runs as uid {uid}");
        }
    }
    let (status, body) = http_get(addr, &format!("/_nanovm/events?app={}&since=0", worker(&hostile))).await;
    assert_eq!(status, 200);
    assert!(body.contains("egress-denied"), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn killed_worker_is_restarted_and_keeps_its_volumes() {
    let (_sup, addr) = supervised("killed_worker_is_restarted").await;
    let (counter, hostile) = (publish("counter", &app("counter")).await, publish("hostile", &app("hostile")).await);
    assert_eq!(app_get(addr, &hostile, "/k/").await.0, 200);
    let incr = format!("POST /k/incr HTTP/1.1\r\nHost: x\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n", names(&counter));
    assert_eq!(raw(addr, &incr, b"").await.0, 200);

    let pid = stats_json(addr).await["workers"][worker(&counter).as_str()]["pid"].as_u64().unwrap();
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    let t0 = Instant::now();
    while stats_json(addr).await["workers"][worker(&counter).as_str()]["up"] == true {
        assert!(t0.elapsed() < Duration::from_secs(2), "worker still up");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Neighbours are untouched.
    assert_eq!(app_get(addr, &hostile, "/k/").await.0, 200);

    // The next request restarts it; `/data` survived.
    let (status, body) = app_get(addr, &counter, "/k/").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""count":1"#), "{body}");
    let v = stats_json(addr).await;
    assert_eq!(v["workers"][worker(&counter).as_str()]["starts"], 2, "{v}");
    assert_ne!(v["workers"][worker(&counter).as_str()]["pid"].as_u64().unwrap(), pid);
}

/// Runs `jail_probe` in a child process (a jail cannot be left), then checks
/// what the jail let it do.
#[cfg(target_os = "linux")]
#[test]
fn jail_blocks_escapes() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("jail-probe");
    let _ = std::fs::remove_dir_all(&dir);
    for d in ["mine", "other"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    std::fs::write(dir.join("other/secret"), "s").unwrap();
    if unsafe { libc::geteuid() } == 0 {
        std::os::unix::fs::chown(dir.join("mine"), Some(200_099), Some(200_099)).unwrap();
    }
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "jail_probe", "--nocapture", "--test-threads", "1"])
        .env("NANOVM_JAIL_PROBE", &dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Stop and continue the probe during its final sleep, as a debugger or
    // job control would: the kernel resumes the sleep via restart_syscall.
    std::thread::sleep(Duration::from_millis(150));
    unsafe {
        libc::kill(child.id() as i32, libc::SIGSTOP);
        std::thread::sleep(Duration::from_millis(20));
        libc::kill(child.id() as i32, libc::SIGCONT);
    }
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let results: std::collections::HashMap<&str, &str> = stdout
        .lines()
        .filter_map(|l| l.rsplit_once("probe ").map(|(_, r)| r))
        .filter_map(|l| l.split_once('='))
        .collect();
    let expect = [
        ("write_own_dir", "ok"),
        ("cwd", "ok"),
        ("thread", "ok"),
        ("read_other_app", "denied"),
        ("read_etc_passwd", "denied"),
        ("list_root", "denied"),
        ("tcp_socket", "denied"),
        ("unix_socket", "denied"),
        ("fork", "denied"),
        ("exec", "denied"),
        ("setuid_root", "denied"),
        ("ptrace", "denied"),
        ("kill_init", "denied"),
        ("unshare", "denied"),
        ("mmap_wx", "denied"),
        ("raise_nofile", "denied"),
        ("signal_owner", "denied"),
        ("sleep_survives_stop", "ok"),
    ];
    for (k, v) in expect {
        assert_eq!(results.get(k).copied(), Some(v), "{k}\n{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    }
}

/// The jailed half of `jail_blocks_escapes`; a no-op when run directly.
#[cfg(target_os = "linux")]
#[test]
fn jail_probe() {
    let Some(dir) = std::env::var_os("NANOVM_JAIL_PROBE").map(PathBuf::from) else {
        return;
    };
    let root = unsafe { libc::geteuid() } == 0;
    nanovm::jail::Jail {
        uid: root.then_some(200_099),
        read_write: vec![dir.join("mine")],
        read_only: vec![],
    }
    .enter()
    .unwrap();
    let report = |name: &str, ok: bool| println!("probe {name}={}", if ok { "ok" } else { "denied" });
    let sys = |r: i64| r >= 0;
    report("write_own_dir", std::fs::write(dir.join("mine/f"), "x").is_ok());
    // A relative data_dir is canonicalized against it.
    report("cwd", std::env::current_dir().is_ok());
    report("thread", std::thread::spawn(|| 1).join().is_ok());
    report("read_other_app", std::fs::read(dir.join("other/secret")).is_ok());
    report("read_etc_passwd", std::fs::read("/etc/passwd").is_ok());
    report("list_root", std::fs::read_dir("/").is_ok());
    unsafe {
        report("tcp_socket", sys(libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) as i64));
        report("unix_socket", sys(libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) as i64));
        let pid = libc::fork();
        if pid == 0 {
            libc::_exit(0);
        }
        report("fork", sys(pid as i64));
        report("exec", std::process::Command::new("/bin/true").status().is_ok());
        // Raw: glibc's setuid broadcasts to every thread of the test harness.
        report("setuid_root", sys(libc::syscall(libc::SYS_setuid, 0)) && libc::getuid() == 0);
        report("ptrace", sys(libc::ptrace(libc::PTRACE_ATTACH, 1, 0, 0)));
        report("kill_init", sys(libc::kill(1, 0) as i64));
        report("unshare", sys(libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) as i64));
        let p = libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        report("mmap_wx", p != libc::MAP_FAILED);
        let lim = libc::rlimit {
            rlim_cur: libc::RLIM_INFINITY,
            rlim_max: libc::RLIM_INFINITY,
        };
        report("raise_nofile", sys(libc::setrlimit(libc::RLIMIT_NOFILE, &lim) as i64));
        // Owning a descriptor's I/O signals would let it signal its supervisor.
        let f = std::fs::File::open(dir.join("mine/f")).unwrap();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&f);
        report("signal_owner", sys(libc::fcntl(fd, libc::F_SETOWN, libc::getppid()) as i64));
    }
    std::thread::sleep(Duration::from_millis(400));
    report("sleep_survives_stop", true);
}

/// Many requests through the supervisor reuse its links to the worker
/// without errors or restarts.
#[tokio::test(flavor = "multi_thread")]
async fn worker_connection_outlives_many_requests() {
    let (_sup, addr) = supervised("worker_connection_outlives_many_requests").await;
    let counter = publish("counter", &app("counter")).await;
    let tasks: Vec<_> = (0..32)
        .map(|t| {
            let counter = counter.clone();
            tokio::spawn(async move {
                let mut bad = 0;
                for i in 0..400 {
                    if app_get(addr, &counter, &format!("/c{}/", (t + i) % 8)).await.0 != 200 {
                        bad += 1;
                    }
                }
                bad
            })
        })
        .collect();
    let mut bad = 0;
    for t in tasks {
        bad += t.await.unwrap();
    }
    assert_eq!(bad, 0);
    let v = stats_json(addr).await;
    assert_eq!(v["workers"][worker(&counter).as_str()]["starts"], 1, "{v}");
    assert_eq!(v["workers"][worker(&counter).as_str()]["link_errors"], 0, "{v}");
}

/// A fresh test cgroup under the cgroup v2 mount, with `controllers` enabled
/// for its children, or why this host can't provide one.
fn test_cgroup(name: &str, controllers: &[&str]) -> Result<PathBuf, String> {
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let mount = mounts
        .lines()
        .find_map(|l| {
            let f: Vec<_> = l.split(' ').collect();
            (f.get(2) == Some(&"cgroup2")).then(|| PathBuf::from(f[1]))
        })
        .ok_or("no cgroup v2 hierarchy (scripts/vm-test.sh boots one)")?;
    let available = std::fs::read_to_string(mount.join("cgroup.controllers")).unwrap_or_default();
    if let Some(c) = controllers.iter().find(|c| !available.split_whitespace().any(|a| a == **c)) {
        return Err(format!(
            "the {c} controller is not available in {} (scripts/vm-test.sh boots a VM where it is)",
            mount.display()
        ));
    }
    let root = mount.join(format!("nanovm-{name}-{}", std::process::id()));
    std::fs::create_dir(&root).map_err(|e| format!("{}: {e}", mount.display()))?;
    let enable: Vec<_> = controllers.iter().map(|c| format!("+{c}")).collect();
    std::fs::write(root.join("cgroup.subtree_control"), enable.join(" ")).map_err(|e| e.to_string())?;
    Ok(root)
}

/// Kills what is left in a test cgroup and removes it.
async fn remove_cgroup(root: &Path) {
    for _ in 0..200 {
        for app in std::fs::read_dir(root).into_iter().flatten().flatten().filter(|e| e.path().is_dir()) {
            let _ = std::fs::write(app.path().join("cgroup.kill"), "1");
            let _ = std::fs::remove_dir(app.path());
        }
        if std::fs::remove_dir(root).is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn cgroup_stat(dir: &Path, file: &str, key: &str) -> u64 {
    let text = std::fs::read_to_string(dir.join(file)).unwrap_or_default();
    text.lines()
        .find_map(|l| l.strip_prefix(key)?.trim().parse().ok())
        .unwrap_or(0)
}

/// Each worker lands in `<cgroup>/<app>`.
#[tokio::test(flavor = "multi_thread")]
async fn workers_are_placed_in_their_cgroups() {
    let root = match test_cgroup("placed", &[]) {
        Ok(r) => r,
        Err(why) => return eprintln!("skipped: {why}"),
    };
    let mut cfg = node_cfg("workers_are_placed_in_their_cgroups", 8);
    cfg.cgroup = Some(root.clone());
    let (sup, addr) = supervised_with(cfg).await;
    let (counter, hostile) = (publish("counter", &app("counter")).await, publish("hostile", &app("hostile")).await);
    for a in [&counter, &hostile] {
        assert_eq!(app_get(addr, a, "/x/").await.0, 200);
    }
    let v = stats_json(addr).await;
    for app in [worker(&counter), worker(&hostile)] {
        let pid = v["workers"][&app]["pid"].as_u64().unwrap();
        let procs = std::fs::read_to_string(root.join(&app).join("cgroup.procs")).unwrap();
        assert!(procs.lines().any(|p| p == pid.to_string()), "{app}: {procs}");
    }
    drop(sup);
    remove_cgroup(&root).await;
}

/// A worker over its `memory.max` is OOM-killed and restarted, while the
/// other app's worker keeps serving.
#[tokio::test(flavor = "multi_thread")]
async fn worker_memory_max_kills_only_that_worker() {
    let root = match test_cgroup("memory", &["memory"]) {
        Ok(r) => r,
        Err(why) => return eprintln!("skipped: {why}"),
    };
    // Each hostile object may hold 32 MiB; eight holding 30 MiB exceed 160.
    let mut cfg = node_cfg("worker_memory_max_kills_only_that_worker", 8);
    cfg.cgroup = Some(root.clone());
    cfg.worker.memory_mb = 160;
    let (sup, addr) = supervised_with(cfg).await;
    let mut big = app("hostile");
    big.limits.cpu_ms = 30_000;
    big.limits.request_timeout_ms = 30_000;
    let (counter, hostile) = (publish("counter", &app("counter")).await, publish("hostile", &big).await);
    assert_eq!(app_get(addr, &hostile, "/x/").await.0, 200);
    let group = root.join(worker(&hostile));
    assert_eq!(std::fs::read_to_string(group.join("memory.max")).unwrap().trim(), (160u64 << 20).to_string());

    let pid = stats_json(addr).await["workers"][worker(&hostile).as_str()]["pid"].as_u64().unwrap();
    let mut statuses = Vec::new();
    for i in 0..8 {
        statuses.push(app_get(addr, &hostile, &format!("/m{i}/alloc?mb=30")).await.0);
    }
    assert!(cgroup_stat(&group, "memory.events", "oom_kill ") >= 1, "no OOM kill: {statuses:?}");
    assert_eq!(app_get(addr, &counter, "/n/").await.0, 200, "the other app kept serving");
    // The next request starts a fresh worker.
    assert_eq!(app_get(addr, &hostile, "/after/").await.0, 200);
    let v = stats_json(addr).await;
    assert_ne!(v["workers"][worker(&hostile).as_str()]["pid"].as_u64().unwrap(), pid, "{v}");
    drop(sup);
    remove_cgroup(&root).await;
}

/// `cpu.max` throttles a worker's spinning guests, and the worker stays
/// within `pids.max`.
#[tokio::test(flavor = "multi_thread")]
async fn worker_cpu_and_pids_limits_hold() {
    let root = match test_cgroup("cpu", &["cpu", "pids"]) {
        Ok(r) => r,
        Err(why) => return eprintln!("skipped: {why}"),
    };
    let mut cfg = node_cfg("worker_cpu_and_pids_limits_hold", 8);
    cfg.cgroup = Some(root.clone());
    cfg.worker.cpus = 0.2;
    cfg.worker.pids = 64;
    let (sup, addr) = supervised_with(cfg).await;
    let hostile = publish("hostile", &app("hostile")).await;
    assert_eq!(app_get(addr, &hostile, "/x/").await.0, 200);
    let group = root.join(worker(&hostile));
    assert_eq!(std::fs::read_to_string(group.join("cpu.max")).unwrap().trim(), "20000 100000");
    assert_eq!(std::fs::read_to_string(group.join("pids.max")).unwrap().trim(), "64");

    let spins: Vec<_> = (0..4)
        .map(|i| {
            let hostile = hostile.clone();
            tokio::spawn(async move { app_get(addr, &hostile, &format!("/spin{i}/spin")).await })
        })
        .collect();
    for s in spins {
        s.await.unwrap();
    }
    assert!(cgroup_stat(&group, "cpu.stat", "nr_throttled ") > 0, "cpu.max did not throttle");
    let pids = cgroup_stat(&group, "pids.current", "");
    assert!((1..=64).contains(&pids), "pids.current = {pids}");
    drop(sup);
    remove_cgroup(&root).await;
}

/// An upstream that answers with headers and the start of a large body,
/// then stalls, so each response body stays open. Returns its port.
async fn stalling_upstream() -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = upstream.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = upstream.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\nstart").await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        }
    });
    port
}

/// `max_outbound_concurrency` (2 here) counts open response bodies, not just
/// requests waiting for headers, both in-process and through the supervisor.
#[tokio::test(flavor = "multi_thread")]
async fn open_response_bodies_count_against_outbound_concurrency() {
    let port = stalling_upstream().await;
    let path = format!("/hold?url=http://127.0.0.1:{port}/&n=3");
    let mut hostile = app("hostile");
    hostile.limits.egress.push(format!("127.0.0.1:{port}"));
    let node = node_with(node_cfg("open_bodies_in_process", 8), hostile.clone());
    let r = get(&node, "h", &path).await;
    assert_eq!(r.json()["held"], 2, "{}", r.body);
    assert!(r.body.contains("ConnectionLimitReached"), "{}", r.body);

    let (_sup, addr) = supervised("open_bodies_supervised").await;
    let (_, body) = app_get(addr, &publish("hostile", &hostile).await, &format!("/h{path}")).await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["held"], 2, "{body}");
    assert!(body.contains("ConnectionLimitReached"), "{body}");
}

// ---- registry apps -------------------------------------------------------
//
// These run against the guests CI publishes (.github/workflows/
// publish-guests.yml) to ghcr.io, signed by GitHub artifact attestations, and
// skip, saying why, where those can't be reached.

const SIGNER: &str = "https://token.actions.githubusercontent.com \
    https://github.com/carsonfarmer/nanovm/.github/workflows/publish-guests.yml@refs/heads/main";

/// The published versions of guest `name`, oldest first, as
/// `(version, ghcr.io/…@digest)`.
/// A drain that runs out of time kills the old worker even while a client
/// still holds one of its responses unread, so it can't run beside the next
/// version. (The rollback from the newest version to the one before is a
/// switch like any other.)
#[tokio::test(flavor = "multi_thread")]
async fn a_drain_that_runs_out_of_time_kills_the_old_worker() {
    use tokio::io::AsyncWriteExt;
    let versions = match published("hostile").await {
        Ok(v) if v.len() >= 2 => v,
        Ok(_) => return eprintln!("skipped: needs two published versions of hostile"),
        Err(why) => return eprintln!("skipped: {why}"),
    };
    let (old, new) = (versions[versions.len() - 2].1.clone(), versions[versions.len() - 1].1.clone());
    let (_sup, addr) = signed("drain_kills_the_old_worker").await;

    // A client that reads a little of a 7 MiB answer, then stops.
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.set_recv_buffer_size(4096).unwrap();
    let mut held = sock.connect(addr).await.unwrap();
    let head = format!(
        "GET /h/big?mb=7 HTTP/1.1\r\nHost: x\r\nnanovm-app: {new}\r\nnanovm-trust: {}\r\n\r\n",
        SIGNER
    );
    held.write_all(head.as_bytes()).await.unwrap();
    let mut response = [0; 64];
    let n = tokio::io::AsyncReadExt::read(&mut held, &mut response).await.unwrap();
    assert!(response[..n].starts_with(b"HTTP/1.1 200"), "{}", String::from_utf8_lossy(&response[..n]));
    let pid = stats_json(addr).await["workers"][worker(&new).as_str()]["pid"].as_u64().unwrap();

    let t0 = Instant::now();
    assert_eq!(reg_get(addr, "/x/", &old, SIGNER, None).await.0, 200);
    // The switch waited out the drain (hostile's 2 s + 1 s), then killed it.
    assert!(t0.elapsed() >= Duration::from_secs(3), "{:?}", t0.elapsed());
    let t1 = Instant::now();
    while std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z ")) {
        assert!(t1.elapsed() < Duration::from_secs(2), "the old worker is still running");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(held);
}

async fn published(name: &str) -> Result<Vec<(semver::Version, String)>, String> {
    use oci_client::{Reference, secrets::RegistryAuth};
    let repo = format!("ghcr.io/carsonfarmer/nanovm-{name}");
    let client = oci_client::Client::default();
    let r: Reference = format!("{repo}:latest").parse().unwrap();
    let tags = client
        .list_tags(&r, &RegistryAuth::Anonymous, None, None)
        .await
        .map_err(|e| format!("can't list {repo}: {e}"))?;
    let mut out = Vec::new();
    for tag in tags.tags {
        let Ok(v) = tag.parse::<semver::Version>() else { continue };
        let r: Reference = format!("{repo}:{tag}").parse().unwrap();
        let accept = [oci_client::manifest::OCI_IMAGE_MEDIA_TYPE];
        let (_, digest) = client
            .pull_manifest_raw(&r, &RegistryAuth::Anonymous, &accept)
            .await
            .map_err(|e| e.to_string())?;
        out.push((v, format!("{repo}@{digest}")));
    }
    out.sort();
    if out.is_empty() {
        return Err(format!("nothing published at {repo}"));
    }
    Ok(out)
}

/// A supervisor that checks signatures.
async fn signed(test: &str) -> (Arc<nanovm::supervisor::Supervisor>, std::net::SocketAddr) {
    let mut cfg = node_cfg(test, 8);
    cfg.dev = false;
    supervised_with(cfg).await
}

async fn reg_get(addr: std::net::SocketAddr, path: &str, app: &str, trust: &str, min: Option<&str>) -> (u16, String) {
    let min = min.map(|m| format!("nanovm-min-version: {m}\r\n")).unwrap_or_default();
    let head = format!(
        "GET {path} HTTP/1.1\r\nHost: x\r\nnanovm-app: {app}\r\nnanovm-trust: {trust}\r\n{min}Connection: close\r\n\r\n"
    );
    raw(addr, &head, b"").await
}

/// A registry app runs from its signed artifact, and anything short of a
/// trusted signature over that digest at an allowed version is refused.
#[tokio::test(flavor = "multi_thread")]
async fn registry_apps_need_a_trusted_signature() {
    let counter = match published("counter").await {
        Ok(v) => v.last().unwrap().clone(),
        Err(why) => return eprintln!("skipped: {why}"),
    };
    let (_sup, addr) = signed("registry_apps_need_a_trusted_signature").await;
    let (version, app) = counter;

    let (status, body) = reg_get(addr, "/a/", &app, SIGNER, Some(&version.to_string())).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(&format!(r#""app":"{}""#, worker(&app))), "{body}");
    let v = stats_json(addr).await;
    assert_eq!(v["workers"][worker(&app).as_str()]["version"], version.to_string(), "{v}");
    // The app's config values are served through wasi:config.
    let (status, body) = reg_get(addr, "/a/config/greeting", &app, SIGNER, None).await;
    assert_eq!((status, body.as_str()), (200, "from the registry"));

    let wrong_subject = SIGNER.replace("publish-guests.yml", "other.yml");
    let wrong_issuer = SIGNER.replace("token.actions", "evil");
    let unsigned = app.rsplit_once('@').unwrap().0.to_string() + "@sha256:" + &"0".repeat(64);
    let too_new = format!("{}.0.0", version.major + 1);
    // The running version, under another policy, is checked again (twice:
    // a refusal is not remembered as an acceptance).
    for (name, a, trust, min, why) in [
        ("subject", app.as_str(), wrong_subject.as_str(), None, "not signed by a trusted identity"),
        ("subject", app.as_str(), wrong_subject.as_str(), None, "not signed by a trusted identity"),
        ("issuer", app.as_str(), wrong_issuer.as_str(), None, "not signed by a trusted identity"),
        ("floor", app.as_str(), SIGNER, Some(too_new.as_str()), "below the minimum"),
        ("digest", unsigned.as_str(), SIGNER, None, "loading"),
    ] {
        let (status, body) = reg_get(addr, "/a/", a, trust, min).await;
        assert_eq!(status, 502, "{name}: {body}");
        assert!(body.contains(why), "{name}: {body}");
    }
    // None of that disturbed the running version.
    assert_eq!(reg_get(addr, "/a/", &app, SIGNER, None).await.0, 200);
    let v = stats_json(addr).await;
    assert_eq!(v["workers"][worker(&app).as_str()]["starts"], 1, "{v}");
    // Trust is required.
    let head = format!("GET /a/ HTTP/1.1\r\nHost: x\r\nnanovm-app: {app}\r\nConnection: close\r\n\r\n");
    assert_eq!(raw(addr, &head, b"").await.0, 400);
}

/// Naming a new version switches the app: requests in flight on the old
/// worker finish, the new worker serves the same volumes.
#[tokio::test(flavor = "multi_thread")]
async fn registry_app_switches_versions_after_a_drain() {
    let versions = match published("hostile").await {
        Ok(v) if v.len() >= 2 => v,
        Ok(_) => return eprintln!("skipped: needs two published versions of hostile"),
        Err(why) => return eprintln!("skipped: {why}"),
    };
    let (old, new) = (&versions[versions.len() - 2], &versions[versions.len() - 1]);
    let (_sup, addr) = signed("registry_app_switches_versions").await;
    let (old_app, new_app) = (old.1.clone(), new.1.clone());

    assert_eq!(reg_get(addr, "/w/fill?mount=data&mb=1", &old_app, SIGNER, None).await.0, 200);
    let slow = tokio::spawn(async move {
        let r = reg_get(addr, "/s/sleep?ms=1500", &old_app, SIGNER, None).await;
        (r, Instant::now())
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The new version sees the old one's volume: 1 of its 8 MiB is used.
    let (status, body) = reg_get(addr, "/w/fill?mount=data&mb=8&name=more.bin", &new_app, SIGNER, None).await;
    let switched = Instant::now();
    assert_eq!(status, 507, "{body}");
    assert!(body.contains(&format!(r#""written":{}"#, 7 << 20)), "{body}");
    let ((slow_status, slow_body), slow_done) = slow.await.unwrap();
    assert_eq!(slow_status, 200, "the old version's request finished: {slow_body}");
    // Two workers never serve the app at once: the new one answers only
    // after the old one's last request is done.
    assert!(switched > slow_done, "the new version answered while the old one was still serving");
    let v = stats_json(addr).await;
    assert_eq!(v["workers"][worker(&new.1).as_str()]["version"], new.0.to_string(), "{v}");
    assert_eq!(v["workers"][worker(&new.1).as_str()]["starts"], 2, "{v}");
}

// ---- wasi:keyvalue -------------------------------------------------------

/// Values survive the instance, live in the object's own volume, and go away
/// when deleted.
#[tokio::test(flavor = "multi_thread")]
async fn kv_values_persist_in_the_volume() {
    let node = node("kv_values_persist", "counter", 8);
    assert_eq!(request(&node, "PUT", "k", "/kv/greeting", "hello").await.status, 204);
    assert_eq!(get(&node, "k", "/kv/greeting").await.body, "hello");
    assert_eq!(get(&node, "other", "/kv/greeting").await.status, 404, "objects don't share keys");

    node.shutdown().await;
    assert_eq!(get(&node, "k", "/kv/greeting").await.body, "hello", "a new instance sees it");
    let file = node.cfg.data_dir.join("objects/counter/k/.kv/default/Z3JlZXRpbmc");
    assert_eq!(std::fs::read(file).unwrap(), b"hello");

    assert_eq!(request(&node, "DELETE", "k", "/kv/greeting", "").await.status, 204);
    assert_eq!(get(&node, "k", "/kv/greeting").await.status, 404);
}

/// Keys count against the object's file limit and values against its disk
/// budget, which `/data` and `/tmp` share.
#[tokio::test(flavor = "multi_thread")]
async fn kv_counts_against_the_disk_quota() {
    let node = node("kv_quota", "hostile", 8);
    // An 8 MiB budget holds exactly eight 1 MiB values.
    let r = get(&node, "q", "/kv?op=fill&size=1048576&n=100").await;
    assert_eq!(r.json()["set"], 8, "{}", r.body);
    assert!(r.body.contains("InsufficientSpace"), "{}", r.body);
    // The filesystem sees the same budget as spent.
    let r = get(&node, "q", "/fill?mount=tmp&mb=2").await;
    assert!(r.body.contains("insufficient-space"), "{}", r.body);

    // 200 entries: the two bucket directories, then one file per key.
    let r = get(&node, "f", "/kv?op=fill&size=1").await;
    assert_eq!(r.json()["set"], 198, "{}", r.body);
    assert!(r.body.contains("Quota"), "{}", r.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn kv_refuses_bad_names_and_pages_keys() {
    // Writing 1,500 keys takes real CPU; give it room on a loaded host.
    let mut hostile = app("hostile");
    hostile.limits.cpu_ms = 5_000;
    hostile.limits.max_files = 3000;
    hostile.limits.request_timeout_ms = 10_000;
    let node = node_with(node_cfg("kv_refuses", 8), hostile);
    let call = |path: &'static str| {
        let node = node.clone();
        async move { get(&node, "b", path).await }
    };
    assert!(call("/kv?op=key&len=180").await.body.contains(r#""set":true"#));
    assert!(call("/kv?op=key&len=181").await.body.contains("keys are 1 to 180 bytes"));
    assert!(call("/kv?op=key&len=0").await.body.contains("keys are 1 to 180 bytes"));
    for name in ["../x", ".kv", "a/b", &"n".repeat(65)] {
        let r = get(&node, "b", &format!("/kv?name={name}")).await;
        assert!(r.body.contains("NoSuchStore"), "{name}: {}", r.body);
    }
    assert_eq!(call("/kv?op=incr&key=c&delta=5").await.json()["value"], 5);
    assert_eq!(call("/kv?op=incr&key=c&delta=5").await.json()["value"], 10);
    assert!(call("/kv?op=incr&key=c&delta=18446744073709551615").await.body.contains("overflow"));
    let not_a_number = "k".repeat(180);
    let r = get(&node, "b", &format!("/kv?op=incr&key={not_a_number}")).await;
    assert!(r.body.contains("not a number"), "{}", r.body);

    let r = get(&node, "p", "/kv?name=paged&op=fill&size=1&n=1500").await;
    assert_eq!(r.json()["set"], 1500, "{}", r.body);
    let page = get(&node, "p", "/kv?name=paged&op=list").await.json();
    assert_eq!((page["keys"].as_u64(), page["cursor"].as_u64()), (Some(1000), Some(1000)));
    let page = get(&node, "p", "/kv?name=paged&op=list&cursor=1000").await.json();
    assert_eq!((page["keys"].as_u64(), page["cursor"].as_u64()), (Some(500), None));
}

/// Key-value works inside the jail (Landlock and seccomp allow its file calls).
#[tokio::test(flavor = "multi_thread")]
async fn kv_works_in_a_jailed_worker() {
    let (_sup, addr) = supervised("kv_works_in_a_jailed_worker").await;
    let (counter, hostile) = (publish("counter", &app("counter")).await, publish("hostile", &app("hostile")).await);
    let put = format!("PUT /j/kv/a HTTP/1.1\r\nHost: x\r\n{}Content-Length: 2\r\nConnection: close\r\n\r\n", names(&counter));
    let (status, body) = raw(addr, &put, b"hi").await;
    assert_eq!(status, 204, "{body}");
    assert_eq!(app_get(addr, &counter, "/j/kv/a").await.1, "hi");
    let delete = format!("DELETE /j/kv/a HTTP/1.1\r\nHost: x\r\n{}Connection: close\r\n\r\n", names(&counter));
    assert_eq!(raw(addr, &delete, b"").await.0, 204);
    assert_eq!(app_get(addr, &counter, "/j/kv/a").await.0, 404);
    let (_, body) = app_get(addr, &hostile, "/j/kv?op=list").await;
    assert!(body.contains(r#""keys":0"#), "{body}");

    // A directory planted over a key's file: the set is refused, and leaves
    // nothing behind that the quota doesn't count.
    let (_, body) = app_get(addr, &hostile, "/p/kv?op=plant").await;
    assert!(body.contains("not a regular file"), "{body}");
    let bucket = std::fs::read_dir(node_dir("kv_works_in_a_jailed_worker").join("objects").join(worker(&hostile)).join("p/.kv/default")).unwrap();
    let names: Vec<_> = bucket.flatten().map(|e| e.file_name()).collect();
    assert_eq!(names, ["azA"], "only the planted directory is there");
}

fn node_dir(test: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("nodes").join(test)
}

/// Reads are bounded by what the guest could hold (its memory limit, 32 MiB
/// here) and batches by size, so a host call can't run away.
#[tokio::test(flavor = "multi_thread")]
async fn kv_reads_and_batches_are_bounded() {
    let node = node("kv_bounded", "hostile", 8);
    let r = get(&node, "m", "/kv?op=fill&size=4194304&n=1").await;
    assert_eq!(r.json()["set"], 1, "{}", r.body);
    // 7 × 4 MiB fits in 32 MiB; 9 doesn't.
    assert_eq!(get(&node, "m", "/kv?op=many&n=7").await.json()["got"], 7);
    let r = get(&node, "m", "/kv?op=many&n=9").await;
    assert!(r.body.contains("memory limit"), "{}", r.body);
    let r = get(&node, "m", "/kv?op=many&n=1001").await;
    assert!(r.body.contains("at most 1000 keys"), "{}", r.body);
}

/// An app's `config` values are what its guests read through `wasi:config`.
#[tokio::test(flavor = "multi_thread")]
async fn wasi_config_serves_the_apps_values() {
    let mut counter = app("counter");
    counter.config.insert("greeting".into(), "hello from config".into());
    let node = node_with(node_cfg("wasi_config", 8), counter);
    let r = get(&node, "c", "/config/greeting").await;
    assert_eq!((r.status, r.body.as_str()), (200, "hello from config"));
    assert_eq!(get(&node, "c", "/config/missing").await.status, 404);
}

// ---- commits ---------------------------------------------------------------

/// A request's writes, to files and to key-value alike, take effect only if
/// it answers, whatever the status; one that fails, traps or runs out of CPU
/// leaves nothing behind.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_requests_writes_are_rolled_back() {
    let node = node("a_failed_requests_writes_are_rolled_back", "hostile", 8);
    let read = async || get(&node, "c", "/commit").await.json();
    let r = get(&node, "c", "/commit?v=one").await;
    assert_eq!(r.status, 200, "{}", r.body);
    for then in ["error", "panic", "spin"] {
        let r = get(&node, "c", &format!("/commit?v=two-{then}&then={then}")).await;
        assert_eq!(r.status, 500, "{}", r.body);
        assert_eq!(read().await, serde_json::json!({"file": "one", "key": "one"}), "after {then}");
    }
    assert_eq!(get(&node, "c", "/commit?v=three&then=fail").await.status, 507);
    assert_eq!(read().await, serde_json::json!({"file": "three", "key": "three"}));
}

/// The journal is out of a guest's reach, and commits work with a relative
/// `data_dir` (the demo's).
#[tokio::test(flavor = "multi_thread")]
async fn the_journal_is_out_of_reach_and_works_from_a_relative_data_dir() {
    let mut cfg = node_cfg("journal_out_of_reach", 8);
    let rel = Path::new("../../target/journal-rel");
    let _ = std::fs::remove_dir_all(rel);
    std::fs::create_dir_all(rel).unwrap();
    std::os::unix::fs::symlink(cfg.data_dir.join("cache"), rel.join("cache")).unwrap();
    cfg.data_dir = rel.to_path_buf();
    let node = node_with(cfg, app("hostile"));
    let r = get(&node, "c", "/commit?v=one").await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = get(&node, "c", "/rename?mount=data&from=../~journal/c&to=j").await;
    assert_eq!(r.status, 507, "{}", r.body);
    assert_eq!(get(&node, "c", "/commit?v=two&then=panic").await.status, 500);
    let r = get(&node, "c", "/commit").await;
    assert_eq!(r.json(), serde_json::json!({"file": "one", "key": "one"}), "{}", r.body);
}

/// A request in flight when its worker dies is rolled back when the object
/// next starts.
#[tokio::test(flavor = "multi_thread")]
async fn a_crash_mid_request_is_rolled_back() {
    let (_sup, addr) = supervised("a_crash_mid_request_is_rolled_back").await;
    let hostile = publish("hostile", &app("hostile")).await;
    assert_eq!(app_get(addr, &hostile, "/c/commit?v=one").await.0, 200);
    let pid = stats_json(addr).await["workers"][worker(&hostile).as_str()]["pid"].as_u64().unwrap() as i32;
    let crashing = tokio::spawn({
        let hostile = hostile.clone();
        async move { app_get(addr, &hostile, "/c/commit?v=two&then=sleep").await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let _ = crashing.await;
    let t0 = Instant::now();
    while stats_json(addr).await["workers"][worker(&hostile).as_str()]["up"] == true {
        assert!(t0.elapsed() < Duration::from_secs(2), "worker still up");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (status, body) = app_get(addr, &hostile, "/c/commit").await;
    assert_eq!((status, body.as_str()), (200, r#"{"file":"one","key":"one"}"#));
}

// ---- calls between objects ---------------------------------------------

/// An object calls another of its app at `http://nanovm/objects/{id}/…`. The
/// callee serves the call from its own queue and volume, and commits before
/// the caller hears back. A call that would wait on its own caller, directly
/// or through another call, is refused at once with 508 instead of
/// deadlocking until a deadline kills both.
#[tokio::test(flavor = "multi_thread")]
async fn objects_call_objects() {
    let node = node("objects_call_objects", "hostile", 8);
    let enc = |s: &str| s.replace('%', "%25").replace('&', "%26").replace('?', "%3F").replace('=', "%3D");
    let call = async |from: &str, url: &str| {
        let r = get(&node, from, &format!("/fetch?url={}", enc(url))).await;
        assert_eq!(r.status, 200, "{}", r.body);
        r.json()
    };

    let r = call("a", "http://nanovm/objects/b/commit?v=via-a").await;
    assert_eq!((r["status"].as_u64(), r["body"].as_str()), (Some(200), Some(r#"{"file":"via-a","key":"via-a"}"#)));
    let r = get(&node, "b", "/commit").await;
    assert_eq!(r.body, r#"{"file":"via-a","key":"via-a"}"#, "the callee's volume has it");
    assert_eq!(get(&node, "a", "/commit").await.body, r#"{"file":"","key":""}"#, "the caller's doesn't");

    let t0 = Instant::now();
    let r = call("a", "http://nanovm/objects/a/").await;
    assert_eq!(r["status"], 508, "{r}");
    let inner = format!("http://nanovm/objects/a/fetch?url={}", enc("http://nanovm/objects/b/"));
    let r = call("b", &inner).await;
    assert!(r["body"].as_str().unwrap().contains(r#""status":508"#), "{r}");
    assert!(t0.elapsed() < Duration::from_secs(1), "refused at once: {:?}", t0.elapsed());

    // Answered calls wait on nothing: b may now call a.
    assert_eq!(call("b", "http://nanovm/objects/a/").await["status"], 200);
}

// ---- WebSockets ----------------------------------------------------------

mod sockets {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{Error, Message, protocol::frame::coding::CloseCode};

    type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

    /// A handshake for `path` on `app`.
    fn handshake(addr: std::net::SocketAddr, app: &str, path: &str) -> http::Request<()> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut req = format!("ws://{addr}{path}").into_client_request().unwrap();
        req.headers_mut().insert("nanovm-app", app.parse().unwrap());
        req.headers_mut().insert("nanovm-trust", "dev dev".parse().unwrap());
        req
    }

    /// The socket, or the refusal's status and body.
    async fn open(addr: std::net::SocketAddr, app: &str, path: &str) -> Result<Ws, (u16, String)> {
        match tokio_tungstenite::connect_async(handshake(addr, app, path)).await {
            Ok((ws, _)) => Ok(ws),
            Err(Error::Http(res)) => {
                Err((res.status().as_u16(), String::from_utf8_lossy(res.body().as_deref().unwrap_or(&[])).into()))
            }
            Err(e) => panic!("{path}: {e}"),
        }
    }

    fn refused(r: Result<Ws, (u16, String)>) -> (u16, String) {
        r.err().expect("refused")
    }

    /// A socket whose client takes little before the kernel stops sending.
    async fn open_slow(addr: std::net::SocketAddr, app: &str, path: &str) -> Ws {
        let sock = tokio::net::TcpSocket::new_v4().unwrap();
        sock.set_recv_buffer_size(4096).unwrap();
        let tcp = tokio_tungstenite::MaybeTlsStream::Plain(sock.connect(addr).await.unwrap());
        tokio_tungstenite::client_async(handshake(addr, app, path), tcp).await.unwrap().0
    }

    /// The next data message or close frame.
    async fn next(ws: &mut Ws) -> Message {
        loop {
            let m = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.expect("a message in time");
            match m.expect("the socket is open").expect("a frame") {
                Message::Ping(_) | Message::Pong(_) => continue,
                m => return m,
            }
        }
    }

    async fn say(ws: &mut Ws, text: &str) {
        ws.send(Message::text(text)).await.unwrap();
    }

    async fn sockets_open(addr: std::net::SocketAddr, n: u64) {
        let t0 = Instant::now();
        while stats_json(addr).await["sockets"] != n {
            assert!(t0.elapsed() < Duration::from_secs(5), "sockets never reached {n}: {}", stats_json(addr).await);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn post(addr: std::net::SocketAddr, app: &str, path: &str) -> u16 {
        let head = format!("POST {path} HTTP/1.1\r\nHost: x\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n", names(app));
        raw(addr, &head, b"").await.0
    }

    /// Events reach the object, which answers on the socket and publishes to
    /// the sockets of its own channel (from an event or a plain request), and
    /// learns how each socket ended unless it closed it itself. Events wait
    /// for room in a full queue, only sockets send them, and handshakes are
    /// checked.
    #[tokio::test(flavor = "multi_thread")]
    async fn websockets_reach_their_object_and_fan_out_within_it() {
        let (_sup, addr) = supervised("websockets_fan_out").await;
        let mut counter = app("counter");
        counter.limits.max_queue = 2;
        let (counter, hostile) = (publish("counter", &counter).await, publish("hostile", &app("hostile")).await);
        let (mut a, mut b) = (open(addr, &counter, "/room/ws").await.unwrap(), open(addr, &counter, "/room/ws").await.unwrap());
        let mut other = open(addr, &counter, "/elsewhere/ws").await.unwrap();
        sockets_open(addr, 3).await;

        say(&mut a, "hello").await;
        assert_eq!(next(&mut a).await, Message::text("echo hello"));
        a.send(Message::binary(vec![0, 1, 2])).await.unwrap();
        assert_eq!(next(&mut a).await, Message::binary(vec![0, 1, 2]));
        say(&mut a, "id").await;
        let Message::Text(id) = next(&mut a).await else { panic!() };
        say(&mut b, "id").await;
        assert_ne!(next(&mut b).await, Message::Text(id));

        say(&mut a, "pub hi").await;
        assert_eq!(next(&mut a).await, Message::text("hi"));
        assert_eq!(next(&mut b).await, Message::text("hi"));
        assert_eq!(post(addr, &counter, "/room/publish?msg=yo").await, 200);
        assert_eq!(next(&mut a).await, Message::text("yo"));
        assert_eq!(next(&mut b).await, Message::text("yo"));
        // What a request publishes is sent only if it answers.
        say(&mut b, "pubpanic lost").await;
        let Message::Close(Some(f)) = next(&mut b).await else { panic!("no close frame") };
        assert_eq!(f.code, CloseCode::Error);
        b = open(addr, &counter, "/room/ws").await.unwrap();
        say(&mut a, "pub kept").await;
        assert_eq!(next(&mut a).await, Message::text("kept"));
        assert_eq!(next(&mut b).await, Message::text("kept"));
        // Another object's sockets heard none of it.
        say(&mut other, "x").await;
        assert_eq!(next(&mut other).await, Message::text("echo x"));

        say(&mut b, "close").await;
        let Message::Close(Some(f)) = next(&mut b).await else { panic!("no close frame") };
        assert_eq!(f.code, CloseCode::from(4000));
        a.close(None).await.unwrap();
        drop(other);
        sockets_open(addr, 0).await;
        // `a`, and the `b` whose event failed; not the `b` the object closed.
        assert_eq!(app_get(addr, &counter, "/room/kv/ws-ends").await, (200, "2".into()));
        assert_eq!(app_get(addr, &counter, "/elsewhere/kv/ws-ends").await, (200, "1".into()));

        // Eight sockets' events at once wait for room in a queue of two.
        let mut many = Vec::new();
        for _ in 0..8 {
            many.push(open(addr, &counter, "/busy/ws").await.unwrap());
        }
        futures::future::join_all(many.iter_mut().map(|ws| say(ws, "hello"))).await;
        for ws in &mut many {
            assert_eq!(next(ws).await, Message::text("echo hello"));
        }
        drop(many);
        sockets_open(addr, 0).await;
        assert_eq!(app_get(addr, &counter, "/busy/kv/ws-ends").await, (200, "8".into()));

        // An object that doesn't answer `OPEN` refuses the socket, with its
        // own status if it gave one.
        assert_eq!(refused(open(addr, &hostile, "/x/").await).0, 502);
        assert_eq!(refused(open(addr, &counter, "/room/nope").await).0, 404);
        // A client can't send events, and handshakes are checked.
        let forged = format!(
            "POST /room/ws HTTP/1.1\r\nHost: x\r\n{}Content-Type: application/websocket-events\r\n\
            Connection-Id: 1\r\nContent-Length: 12\r\nConnection: close\r\n\r\nDISCONNECT\r\n",
            names(&counter)
        );
        assert_eq!(raw(addr, &forged, b"").await.0, 400);
        let old = format!(
            "GET /room/ws HTTP/1.1\r\nHost: x\r\n{}Upgrade: websocket\r\nConnection: Upgrade, close\r\n\
            Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 8\r\n\r\n",
            names(&counter)
        );
        assert_eq!(raw(addr, &old, b"").await.0, 426);
        assert_eq!(app_get(addr, &counter, "/room/kv/ws-ends").await, (200, "2".into()));
        sockets_open(addr, 0).await;
    }

    /// Sockets per object, per app and per node are capped, a message is
    /// bounded by the request limit, a client that falls behind is dropped,
    /// and a worker's sockets close with it.
    #[tokio::test(flavor = "multi_thread")]
    async fn websocket_limits_hold() {
        let mut cfg = node_cfg("websocket_limits_hold", 8);
        cfg.max_sockets = 4;
        cfg.worker.max_sockets = 3;
        let (_sup, addr) = supervised_with(cfg).await;
        let mut counter = app("counter");
        let limits = &mut counter.limits;
        (limits.max_sockets, limits.max_request_body_kb, limits.max_response_body_kb) = (2, 16, 64);
        let (counter, chat) = (publish("counter", &counter).await, publish("chat", &counter).await);

        let a1 = open_slow(addr, &counter, "/a/ws").await;
        let _a2 = open_slow(addr, &counter, "/a/ws").await;
        let (status, body) = refused(open(addr, &counter, "/a/ws").await);
        assert_eq!((status, body.contains("object has max_sockets")), (503, true), "{body}");
        let mut big = open(addr, &counter, "/b/ws").await.unwrap();
        let (status, body) = refused(open(addr, &counter, "/c/ws").await);
        assert_eq!((status, body.contains("app has max_sockets")), (503, true), "{body}");
        let other_app = open(addr, &chat, "/x/ws").await.unwrap();
        let (status, body) = refused(open(addr, &chat, "/y/ws").await);
        assert_eq!((status, body.contains("node has max_sockets")), (503, true), "{body}");
        drop(other_app);
        sockets_open(addr, 3).await;

        big.send(Message::text("x".repeat(16 << 10))).await.unwrap();
        let Message::Close(Some(f)) = next(&mut big).await else { panic!("no close frame") };
        assert_eq!(f.code, CloseCode::Size);
        sockets_open(addr, 2).await;

        // `a1` and `a2` never read; what is published to them piles up until
        // they are dropped.
        let msg = "y".repeat(1000);
        for i in 0.. {
            assert_eq!(post(addr, &counter, &format!("/a/publish?msg={msg}&n=50")).await, 200);
            if stats_json(addr).await["sockets"] == 0 {
                break;
            }
            assert!(i < 2000, "a socket that never reads was never dropped");
        }
        assert_eq!(app_get(addr, &counter, "/a/kv/ws-ends").await, (200, "2".into()));
        drop(a1);

        // A client that gives up while its `OPEN` waits on the object still
        // has its socket ended for the object, which accepted it.
        let pid = stats_json(addr).await["workers"][worker(&counter).as_str()]["pid"].as_u64().unwrap() as i32;
        unsafe { libc::kill(pid, libc::SIGSTOP) };
        let upgrade = format!(
            "GET /e/ws HTTP/1.1\r\nHost: x\r\n{}Upgrade: websocket\r\nConnection: Upgrade\r\n\
            Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
            names(&counter)
        );
        let mut gave_up = tokio::net::TcpStream::connect(addr).await.unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut gave_up, upgrade.as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(gave_up);
        unsafe { libc::kill(pid, libc::SIGCONT) };
        let t0 = Instant::now();
        while app_get(addr, &counter, "/e/kv/ws-ends").await != (200, "1".into()) {
            assert!(t0.elapsed() < Duration::from_secs(5), "the object never heard the socket end");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // A worker's sockets close when it goes.
        let mut s = open(addr, &counter, "/d/ws").await.unwrap();
        let pid = stats_json(addr).await["workers"][worker(&counter).as_str()]["pid"].as_u64().unwrap();
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        let Message::Close(Some(f)) = next(&mut s).await else { panic!("no close frame") };
        assert_eq!(f.code, CloseCode::Restart);
        sockets_open(addr, 0).await;
    }
}

// ---- the proxy -------------------------------------------------------------

/// A DNS-over-HTTPS stub answering TXT queries from `records`, by name.
async fn doh(records: Arc<parking_lot::Mutex<std::collections::HashMap<String, Vec<String>>>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(nanovm::server::serve_http1(listener, move |req: http::Request<hyper::body::Incoming>| {
        let q = req.uri().query().unwrap_or("");
        let name = q.split('&').find_map(|kv| kv.strip_prefix("name=")).unwrap_or("").to_string();
        let answer: Vec<_> = records.lock().get(&name).into_iter().flatten().map(|txt| {
            serde_json::json!({ "name": name, "type": 16, "TTL": 1, "data": format!("\"{txt}\"") })
        }).collect();
        async move { nanovm::server::ok_json(serde_json::json!({ "Status": 0, "Answer": answer })) }
    }));
    format!("http://{addr}/dns-query")
}

/// A dev-mode proxy in front of `node`, asking `resolver`, and asking the
/// registry for a tag's digest every time.
async fn proxied(node: std::net::SocketAddr, resolver: String) -> std::net::SocketAddr {
    let cfg = nanovm::config::ProxyConfig {
        node: node.to_string(),
        resolver,
        tag_ttl_s: 0,
        dev: true,
        listen: "127.0.0.1:0".into(),
        ..Default::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(nanovm::proxy::serve(nanovm::proxy::Proxy::new(cfg).unwrap(), listener));
    addr
}

/// The proxy serves each host the app its `_nanovm` record names, at the
/// version its tag names now; a client can name no app itself, nor reach
/// the node's own routes; WebSockets pass through.
#[tokio::test(flavor = "multi_thread")]
async fn the_proxy_routes_hosts_by_dns() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
    let (_sup, node) = supervised("the_proxy_routes_hosts_by_dns").await;
    let records = Arc::new(parking_lot::Mutex::default());
    let proxy = proxied(node, doh(records.clone()).await).await;
    let (counter, hostile) = (publish("counter", &app("counter")).await, publish("hostile", &app("hostile")).await);
    let tagged = format!("{}:latest", counter.split_once('@').unwrap().0);
    let record = |host: &str, txt: &[&str]| {
        records.lock().insert(format!("_nanovm.{host}"), txt.iter().map(|t| t.to_string()).collect());
    };
    record("counter.test", &[&format!("app={tagged}; trust=dev dev")]);
    let at = |host: &str, app: &str| nanovm::registry::app_name(host, &app.parse().unwrap());
    let get = async |host: &str, path: &str, extra: &str| {
        raw(proxy, &format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{extra}Connection: close\r\n\r\n"), b"").await
    };

    let (status, body) = get("counter.test", "/a/", "").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(&format!(r#""app":"{}","object":"a""#, at("counter.test", &counter))), "{body}");
    // What a client says its app is goes unheard.
    let (status, body) = get("Counter.Test:8443", "/a/", &names(&hostile)).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(&at("counter.test", &counter)), "{body}");
    assert_eq!(get("counter.test", "/_nanovm/stats", "").await.0, 400);
    // Another host naming the same repository runs an app of its own: it
    // can choose the version it runs, but not the one counter.test runs.
    record("other.test", &[&format!("app={tagged}; trust=dev dev")]);
    let (status, body) = get("other.test", "/a/", "").await;
    assert!(status == 200 && body.contains(&at("other.test", &counter)), "{body}");

    // No record is no app; a bad record, or two, is the proxy's error.
    assert_eq!(get("nowhere.test", "/a/", "").await.0, 404);
    record("bad.test", &["app=x"]);
    record("two.test", &[&format!("app={tagged}; trust=dev dev"), &format!("app={tagged}; trust=dev dev")]);
    for host in ["bad.test", "two.test"] {
        assert_eq!(get(host, "/a/", "").await.0, 502, "{host}");
    }

    // Moving the tag deploys: the next request runs the new version.
    let mut next = app("counter");
    next.config.insert("greeting".into(), "deployed".into());
    let next = publish("counter", &next).await;
    assert_eq!(get("counter.test", "/a/config/greeting", "").await, (200, "deployed".into()));
    let digest = next.split_once('@').unwrap().1;
    assert_eq!(stats_json(node).await["workers"][at("counter.test", &counter).as_str()]["digest"], digest);

    let mut req = format!("ws://{proxy}/room/ws").into_client_request().unwrap();
    req.headers_mut().insert("host", "counter.test".parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(Message::text("hello")).await.unwrap();
    let echo = async {
        loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Ping(_) => continue,
                m => return m,
            }
        }
    };
    assert_eq!(tokio::time::timeout(Duration::from_secs(5), echo).await.unwrap(), Message::text("echo hello"));
}

// ---- apps from the registry, in dev mode ----------------------------------

/// The node downloads no component over `max_component_mb`, and no config
/// (or manifest, or signature) over 4 MiB.
#[tokio::test(flavor = "multi_thread")]
async fn registry_downloads_are_bounded() {
    let mut cfg = node_cfg("registry_downloads_are_bounded", 8);
    cfg.max_component_mb = 0;
    let (_sup, addr) = supervised_with(cfg).await;
    let (status, body) = app_get(addr, &publish("counter", &app("counter")).await, "/a/").await;
    assert_eq!(status, 502, "{body}");
    assert!(body.contains("the component") && body.contains("over 0"), "{body}");

    let (_sup, addr) = supervised("registry_downloads_are_bounded_config").await;
    let mut huge = app("counter");
    huge.config.insert("k".into(), "x".repeat(4 << 20));
    let (status, body) = app_get(addr, &publish("counter", &huge).await, "/a/").await;
    assert_eq!(status, 502, "{body}");
    assert!(body.contains(&format!("over {}", 4 << 20)), "{body}");
}

/// A running version is served under a new policy only once that policy
/// accepts it; a refusal is not remembered as an acceptance, and leaves the
/// version running.
#[tokio::test(flavor = "multi_thread")]
async fn a_running_version_is_checked_against_each_policy() {
    let (_sup, addr) = supervised("a_running_version_is_checked_against_each_policy").await;
    let counter = publish("counter", &app("counter")).await;
    let with_floor = |min: &str| {
        format!(
            "GET /a/ HTTP/1.1\r\nHost: x\r\n{}nanovm-min-version: {min}\r\nConnection: close\r\n\r\n",
            names(&counter)
        )
    };
    assert_eq!(app_get(addr, &counter, "/a/").await.0, 200);
    for _ in 0..2 {
        let (status, body) = raw(addr, &with_floor("2.0.0"), b"").await;
        assert_eq!(status, 502, "{body}");
        assert!(body.contains("below the minimum 2.0.0"), "{body}");
    }
    assert_eq!(raw(addr, &with_floor("1.0.0"), b"").await.0, 200);
    let v = stats_json(addr).await;
    assert_eq!(v["workers"][worker(&counter).as_str()]["starts"], 1, "{v}");
}

/// Nothing a registry sends reaches the node's disk before its app is
/// authorized: an app refused (here for its version floor) leaves the
/// registry cache empty, so unsigned manifests can't fill the disk.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_app_leaves_nothing_on_disk() {
    let test = "a_refused_app_leaves_nothing_on_disk";
    let (_sup, addr) = supervised(test).await;
    let counter = publish("counter", &app("counter")).await;
    let head = format!(
        "GET /a/ HTTP/1.1\r\nHost: x\r\n{}nanovm-min-version: 9.0.0\r\nConnection: close\r\n\r\n",
        names(&counter)
    );
    let (status, body) = raw(addr, &head, b"").await;
    assert!(status == 502 && body.contains("below the minimum"), "{status} {body}");
    let cache = Path::new(env!("CARGO_TARGET_TMPDIR")).join("nodes").join(test).join("registry");
    let kept: Vec<_> = std::fs::read_dir(&cache).into_iter().flatten().flatten().map(|e| e.file_name()).collect();
    assert!(kept.is_empty(), "{kept:?}");
}

/// A switch, once begun, finishes even if the request that began it goes
/// away mid-drain: the old worker is killed and the new one serves, so the
/// app never runs in two.
#[tokio::test(flavor = "multi_thread")]
async fn a_switch_finishes_when_its_request_goes_away() {
    use tokio::io::AsyncWriteExt;
    let (_sup, addr) = supervised("a_switch_finishes_when_its_request_goes_away").await;
    let old = publish("hostile", &app("hostile")).await;
    let mut next = app("hostile");
    next.config.insert("v".into(), "2".into());
    let new = publish("hostile", &next).await;
    assert_eq!(app_get(addr, &old, "/x/").await.0, 200);
    let slow = tokio::spawn({
        let old = old.clone();
        async move { app_get(addr, &old, "/s/sleep?ms=800").await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let pid = stats_json(addr).await["workers"][worker(&old).as_str()]["pid"].as_u64().unwrap();
    // A client asks for the new version, then hangs up during the drain.
    let mut gone = tokio::net::TcpStream::connect(addr).await.unwrap();
    let head = format!("GET /x/ HTTP/1.1\r\nHost: x\r\n{}\r\n", names(&new));
    gone.write_all(head.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(gone);
    assert_eq!(slow.await.unwrap().0, 200);
    let t0 = Instant::now();
    loop {
        let v = stats_json(addr).await;
        let w = &v["workers"][worker(&new).as_str()];
        if w["digest"] == new.split_once('@').unwrap().1 && w["up"] == true {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(5), "the switch never finished: {v}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
    assert!(stat.map_or(true, |s| s.contains(") Z ")), "the old worker still runs");
}

/// At most `max_apps` workers run. A new app takes the slot of the least
/// recently used idle app, never a busy one's; with none idle it is refused,
/// and served once one is.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_runs_at_most_max_apps() {
    let mut cfg = node_cfg("a_node_runs_at_most_max_apps", 8);
    cfg.max_apps = 2;
    let (_sup, addr) = supervised_with(cfg).await;
    let mut apps = Vec::new();
    for repo in ["apps-a", "apps-b", "apps-c"] {
        apps.push(publish(repo, &app("hostile")).await);
    }
    let [a, b, c] = &apps[..] else { unreachable!() };
    let busy = |app: &String, ms: u64| {
        let app = app.clone();
        tokio::spawn(async move { app_get(addr, &app, &format!("/s/sleep?ms={ms}")).await })
    };
    let up = async |app: &str| stats_json(addr).await["workers"][worker(app).as_str()]["up"] == true;

    assert_eq!(app_get(addr, a, "/x/").await.0, 200);
    assert_eq!(app_get(addr, b, "/x/").await.0, 200);
    // `a` is idle and least recently used; `b` is busy.
    let slow_b = busy(b, 900);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(app_get(addr, c, "/x/").await.0, 200);
    assert!(!up(a).await && up(b).await && up(c).await, "{}", stats_json(addr).await);
    // Both running apps busy: `a` is refused, and neither is disturbed.
    let slow_c = busy(c, 600);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (status, body) = app_get(addr, a, "/x/").await;
    assert!(status == 502 && body.contains("most apps (2)"), "{status} {body}");
    assert_eq!(slow_b.await.unwrap().0, 200);
    assert_eq!(slow_c.await.unwrap().0, 200);
    // Once one is idle (and the refusal's backoff has passed), `a` runs.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(app_get(addr, a, "/x/").await.0, 200);
    assert!(up(a).await && !up(b).await && up(c).await, "{}", stats_json(addr).await);
    // Apps that never start are forgotten once there are 2 × max_apps.
    for i in 0..6 {
        let bogus = format!("127.0.0.1:1/nothing/n{i}@sha256:{}", "a".repeat(64));
        assert_eq!(app_get(addr, &bogus, "/x/").await.0, 502);
    }
    let v = stats_json(addr).await;
    let known = v["workers"].as_object().unwrap();
    assert!(known.len() <= 4, "{v}");
    assert_eq!(known.values().filter(|w| w["up"] == true).count(), 2, "{v}");
}

/// A registry that answers a byte at a time can't hold the node: at most
/// four fetches run at once, across apps, and each ends at
/// `load_timeout_ms`.
#[tokio::test(flavor = "multi_thread")]
async fn slow_registries_are_bounded() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let slow = listener.local_addr().unwrap();
    let (open, most) = (Arc::new(std::sync::atomic::AtomicUsize::new(0)), Arc::new(std::sync::atomic::AtomicUsize::new(0)));
    let (o, m) = (open, most.clone());
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
            let (o, m) = (o.clone(), m.clone());
            tokio::spawn(async move {
                m.fetch_max(o.fetch_add(1, Relaxed) + 1, Relaxed);
                let _ = conn.read(&mut [0; 4096]).await;
                let mut out = b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\n".to_vec();
                while conn.write_all(&out).await.is_ok() {
                    out = b"x".to_vec();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                o.fetch_sub(1, Relaxed);
            });
        }
    });
    let mut cfg = node_cfg("slow_registries_are_bounded", 8);
    cfg.load_timeout_ms = 1000;
    let (_sup, addr) = supervised_with(cfg).await;
    let t0 = Instant::now();
    let loads: Vec<_> = (0..8)
        .map(|i| {
            let app = format!("{slow}/slow/app{i}@sha256:{}", "a".repeat(64));
            tokio::spawn(async move { app_get(addr, &app, "/x/").await })
        })
        .collect();
    // Before any deadline, so dropped connections can't be counted.
    tokio::time::sleep(Duration::from_millis(900)).await;
    let most = most.load(Relaxed);
    assert!((1..=4).contains(&most), "{most} fetches at once");
    for load in loads {
        let (status, body) = tokio::time::timeout(Duration::from_secs(10), load).await.expect("a load hangs").unwrap();
        assert!(status == 502 && body.contains("took over 1s"), "{status} {body}");
    }
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
}

/// The public proxy outlasts clients that hold connections: one that never
/// finishes its TLS handshake or its headers is hung up on after 10 s, and
/// running out of file descriptors pauses accepting rather than ending the
/// process.
#[tokio::test(flavor = "multi_thread")]
async fn the_proxy_outlasts_idle_and_excess_connections() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("proxy_outlasts");
    std::fs::create_dir_all(&dir).unwrap();
    let port = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    // A proxy whose resolver is unreachable: every request is its 502.
    let start = |name: &str, extra: &str| {
        let addr = port();
        let cfg = dir.join(format!("{name}.toml"));
        let toml = format!("listen = \"{addr}\"\nresolver = \"http://127.0.0.1:1/dns-query\"\n{extra}");
        std::fs::write(&cfg, toml).unwrap();
        let cmd = format!("ulimit -n 64 && exec {} proxy --config {}", env!("CARGO_BIN_EXE_nanovm"), cfg.display());
        let child = tokio::process::Command::new("sh").args(["-c", &cmd]).kill_on_drop(true).spawn().unwrap();
        (child, addr)
    };
    let connect = async |addr| {
        for _ in 0..100 {
            if let Ok(c) = tokio::net::TcpStream::connect(addr).await {
                return c;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the proxy never listened on {addr}");
    };
    // Waits for the server to hang up, and says how long that took.
    let hung_up = async |mut c: tokio::net::TcpStream| {
        let t0 = Instant::now();
        let n = tokio::time::timeout(Duration::from_secs(20), c.read(&mut [0; 64])).await;
        assert!(matches!(n, Ok(Ok(0)) | Ok(Err(_))), "still open: {n:?}");
        t0.elapsed()
    };

    let (mut plain, addr) = start("plain", "");
    let mut slow = connect(addr).await;
    slow.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n").await.unwrap();
    let headers = tokio::spawn(async move { hung_up(slow).await });
    let tls = std::process::Command::new("openssl")
        .args(["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes"])
        .args(["-days", "1", "-subj", "/CN=localhost", "-keyout", "key.pem", "-out", "cert.pem"])
        .current_dir(&dir)
        .output();
    let handshake = match tls {
        Ok(out) if out.status.success() => {
            let extra = format!("tls_cert = {:?}\ntls_key = {:?}\n", dir.join("cert.pem"), dir.join("key.pem"));
            let (child, addr) = start("tls", &extra);
            let silent = connect(addr).await;
            Some((child, tokio::spawn(async move { hung_up(silent).await })))
        }
        _ => {
            eprintln!("skipped the TLS handshake check: no openssl");
            None
        }
    };

    // Past the fd limit, then back under it: the proxy still answers.
    let mut excess = Vec::new();
    for _ in 0..100 {
        excess.push(connect(addr).await);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(excess);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(plain.try_wait().unwrap().is_none(), "the proxy exited");
    let (status, body) = raw(addr, "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n", b"").await;
    assert_eq!(status, 502, "{body}");

    let took = headers.await.unwrap();
    assert!(took >= Duration::from_secs(9), "{took:?}");
    if let Some((_child, handshake)) = handshake {
        let took = handshake.await.unwrap();
        assert!(took >= Duration::from_secs(9), "{took:?}");
    }
}
