//! End-to-end tests against the real guests in `guests/`. Build them first
//! with `scripts/build-guests.sh`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use nanovm::config::NodeConfig;
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

fn guest(name: &str) -> PathBuf {
    let p = repo()
        .join("guests/target/wasm32-wasip2/release")
        .join(format!("{name}.wasm"));
    assert!(p.exists(), "{} is missing: run scripts/build-guests.sh", p.display());
    p
}

/// A node with the `counter` and `hostile` apps and its own data directory.
/// The compile cache is shared between tests.
fn node(test: &str, max_live: usize, counter_idle_ms: u64) -> Arc<Node> {
    node_with(test, max_live, counter_idle_ms, 100)
}

/// Like `node`, with the hostile app's CPU budget set to `hostile_cpu_ms`.
fn node_with(test: &str, max_live: usize, counter_idle_ms: u64, hostile_cpu_ms: u64) -> Arc<Node> {
    Node::new(node_cfg(test, max_live, counter_idle_ms, hostile_cpu_ms)).unwrap()
}

fn node_cfg(test: &str, max_live: usize, counter_idle_ms: u64, hostile_cpu_ms: u64) -> NodeConfig {
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
        disk_mb = 1
        max_files = 100

        [[app]]
        name = "hostile"
        component = "{hostile}"
        static_dir = "{statics}"
        [app.limits]
        memory_mb = 32
        cpu_ms = {hostile_cpu_ms}
        request_timeout_ms = 1000
        post_return_timeout_ms = 500
        disk_mb = 8
        max_files = 200
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
    cfg
}

async fn get(node: &Arc<Node>, app: &str, id: &str, path: &str) -> Reply {
    request(node, "GET", app, id, path, "").await
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
    let node = node("volume_survives_eviction", 2, 60_000);
    for n in 1..=3 {
        let r = request(&node, "POST", "counter", "a", "/incr", "").await;
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
    assert_eq!(
        request(&node, "PUT", "counter", "x", "/kv/k", "secret").await.status,
        204
    );
    let r = get(&node, "counter", "x", "/kv/k").await;
    assert_eq!((r.status, r.body.as_str()), (200, "secret"));
    let r = get(&node, "counter", "y", "/kv/k").await;
    assert_eq!(r.status, 404, "{}", r.body);
    assert_eq!(request(&node, "DELETE", "counter", "x", "/kv/k", "").await.status, 204);
    assert_eq!(get(&node, "counter", "x", "/kv/k").await.status, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn app_dir_is_readable() {
    let node = node("app_dir_is_readable", 8, 60_000);
    let r = get(&node, "counter", "s", "/static").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        r.body,
        std::fs::read_to_string(repo().join("demo/static/hello.txt")).unwrap()
    );
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
        assert_eq!(r.status, 200, "{}", r.body);
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
async fn killed(node: &Arc<Node>, id: &str, path: &str, status: u16, reason: &str) -> Reply {
    let before = get(node, "hostile", id, "/").await;
    assert_eq!(before.status, 200, "{:?} {}", before.error, before.body);
    let r = get(node, "hostile", id, path).await;
    assert_eq!(
        (r.status, r.error.as_deref()),
        (status, Some(reason)),
        "{path}: {}",
        r.body
    );
    let after = get(node, "hostile", id, "/").await;
    assert_eq!(after.status, 200, "{:?} {}", after.error, after.body);
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

/// Regression: CPU charged to the killed request used to carry over into the
/// next instantiation, so an epoch tick during it killed the new instance
/// at birth (503 instantiate-failed). A 1 ms tick makes that tick likely.
#[tokio::test(flavor = "multi_thread")]
async fn cpu_kill_does_not_poison_next_instance() {
    let mut cfg = node_cfg("cpu_kill_does_not_poison_next_instance", 8, 60_000, 100);
    cfg.epoch_tick_ms = 1;
    let node = Node::new(cfg).unwrap();
    for i in 0..60 {
        let r = get(&node, "hostile", "k", "/spin").await;
        assert_eq!(r.error.as_deref(), Some("cpu"), "{}", r.body);
        let r = get(&node, "hostile", "k", "/").await;
        assert_eq!(r.status, 200, "round {i}: {:?} {}", r.error, r.body);
    }
}

/// Regression: instantiation was charged wall time since its slot was
/// acquired, including host work before the guest ran (scanning `/data`),
/// so an epoch tick during start-up could kill a new instance that had
/// used no CPU. Here the scan alone outlasts the 10 ms budget.
#[tokio::test(flavor = "multi_thread")]
async fn start_up_is_charged_guest_cpu_not_host_work() {
    let mut cfg = node_cfg("start_up_is_charged_guest_cpu", 8, 60_000, 10);
    cfg.epoch_tick_ms = 1;
    let vol = cfg.data_dir.join("objects/hostile/s");
    std::fs::create_dir_all(&vol).unwrap();
    for i in 0..20_000 {
        std::fs::write(vol.join(format!("f{i}")), b"").unwrap();
    }
    let node = Node::new(cfg).unwrap();
    for i in 0..30 {
        let r = get(&node, "hostile", "s", "/").await;
        assert_eq!(r.status, 200, "round {i}: {:?} {}", r.error, r.body);
        assert_eq!(get(&node, "hostile", "s", "/panic").await.status, 500);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn wall_clock_limit() {
    let node = node("wall_clock_limit", 8, 60_000);
    let r = killed(&node, "slow", "/sleep?ms=60000", 504, "timeout").await;
    assert!(
        r.elapsed >= Duration::from_millis(1000) && r.elapsed < Duration::from_millis(2500),
        "took {:?}",
        r.elapsed
    );
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
                    let r = get(&node, "hostile", &format!("hog{i}"), "/spin").await;
                    assert_eq!(r.error.as_deref(), Some("cpu"), "{}", r.body);
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
    let node = node_with("file_count_limit", 8, 60_000, 1_000);
    let r = get(&node, "hostile", "f", "/files?mount=tmp").await;
    assert_eq!(r.status, 507, "{}", r.body);
    assert_eq!(r.json()["created"], 200);
    assert_eq!(r.json()["error"], "quota");
}

#[tokio::test(flavor = "multi_thread")]
async fn disk_quota_and_tmp_wipe() {
    let node = node("disk_quota_and_tmp_wipe", 8, 60_000);
    let fill = async |name: &str, mount: &str, mb: u32| {
        get(
            &node,
            "hostile",
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
    assert_eq!(get(&node, "hostile", "t", "/panic").await.status, 500);
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
    let node = node("rename_quota_accounting", 8, 60_000);
    for name in ["a", "b"] {
        let r = get(&node, "hostile", "r", &format!("/fill?mount=tmp&mb=4&name={name}")).await;
        assert_eq!(r.status, 200, "{}", r.body);
    }
    // Renaming a file onto itself is a no-op and must not credit the quota.
    let r = get(&node, "hostile", "r", "/rename?mount=tmp&from=a&n=10").await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = get(&node, "hostile", "r", "/fill?mount=tmp&mb=1&name=c").await;
    assert_eq!(r.status, 507, "{}", r.body);
    assert_eq!(r.json()["error"], "insufficient-space");
    // Renaming `c` (empty, from the refused fill) over `a` frees a's 4 MiB.
    let r = get(&node, "hostile", "r", "/rename?mount=tmp&from=c&to=a").await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = get(&node, "hostile", "r", "/fill?mount=tmp&mb=4&name=d").await;
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
    assert_eq!(r.status, 507, "{}", r.body);
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
async fn rename_across_mounts_keeps_one_budget() {
    let node = node("rename_across_mounts_keeps_one_budget", 8, 60_000);
    let r = get(&node, "hostile", "r", "/cross-rename").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(node.cfg.data_dir.join("objects/hostile/r/x.txt").exists());
    // The moved file is still counted once: 8 MiB minus its byte still fit.
    let r = get(&node, "hostile", "r", "/fill?mount=data&mb=4&name=a").await;
    assert_eq!(r.status, 200, "{}", r.body);
}

// ---- egress -----------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn egress_unlisted_host_denied() {
    let node = node("egress_unlisted_host_denied", 8, 60_000);
    let r = get(
        &node,
        "hostile",
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
    // An IP-literal allowlist entry is the operator's explicit choice, so it
    // passes the public-IP filter; this exercises the whole send path.
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
    let mut cfg = node_cfg("egress_allowed_request_round_trips", 8, 60_000, 100);
    cfg.apps[1].limits.egress.push(format!("127.0.0.1:{port}"));
    let node = Node::new(cfg).unwrap();
    let r = get(
        &node,
        "hostile",
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
    let node = node("egress_loopback_denied", 8, 60_000);
    // Something is listening, so only the IP filter can stop the request.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let r = get(&node, "hostile", "n", &format!("/fetch?url=http://localhost:{port}/")).await;
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
    eventually("both objects busy", || node.phases().get("handling") == Some(&2)).await;
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
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

#[tokio::test(flavor = "multi_thread")]
async fn http_front_end() {
    let node = node("http_front_end", 8, 60_000);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(nanovm::server::serve(node.clone(), listener));

    let (status, body) = raw(
        addr,
        "GET /counter/web/ HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        b"",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""object":"web""#), "{body}");

    let (status, _) = raw(
        addr,
        "GET /counter HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        b"",
    )
    .await;
    assert_eq!(status, 404);

    let big = vec![b'x'; 2 << 20];
    let head = format!(
        "POST /counter/web/kv/big HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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

/// Starts a supervisor (one jailed worker per app) behind an HTTP listener.
async fn supervised(test: &str) -> (Arc<nanovm::supervisor::Supervisor>, std::net::SocketAddr) {
    supervised_with(node_cfg(test, 8, 60_000, 100)).await
}

async fn supervised_with(cfg: NodeConfig) -> (Arc<nanovm::supervisor::Supervisor>, std::net::SocketAddr) {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_nanovm"));
    let sup = nanovm::supervisor::Supervisor::new(cfg, exe, 1).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let s = sup.clone();
    tokio::spawn(nanovm::server::serve_http1(listener, move |req| s.clone().handle(req)));
    (sup, addr)
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

    let (status, body) = http_get(addr, "/counter/s/").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""object":"s""#), "{body}");

    for (path, error) in [
        ("/hostile/s/alloc?mb=1024", r#""error":"memory""#),
        ("/hostile/s/spin", r#""error":"cpu""#),
        ("/hostile/s/panic", r#""error":"trap""#),
        // The supervisor applies the allowlist and the IP filter.
        ("/hostile/s/fetch?url=http://169.254.169.254/", "HttpRequestDenied"),
        ("/hostile/s/fetch?url=http://localhost:9/", "DestinationIpProhibited"),
        ("/hostile/s/escape", "not-permitted"),
    ] {
        let (_, body) = http_get(addr, path).await;
        assert!(body.contains(error), "{path}: {body}");
    }
    // The worker survived all of it.
    let (status, _) = http_get(addr, "/hostile/s/").await;
    assert_eq!(status, 200);

    let v = stats_json(addr).await;
    assert_eq!(v["apps"]["hostile"]["egress_denied"], 2, "{v}");
    for app in ["counter", "hostile"] {
        let pid = v["workers"][app]["pid"].as_u64().unwrap();
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
    let (status, body) = http_get(addr, "/_nanovm/events?app=hostile&since=0").await;
    assert_eq!(status, 200);
    assert!(body.contains("egress-denied"), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn killed_worker_is_restarted_and_keeps_its_volumes() {
    let (_sup, addr) = supervised("killed_worker_is_restarted").await;
    let incr = "POST /counter/k/incr HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    assert_eq!(raw(addr, incr, b"").await.0, 200);

    let pid = stats_json(addr).await["workers"]["counter"]["pid"].as_u64().unwrap();
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    let t0 = Instant::now();
    while stats_json(addr).await["workers"]["counter"]["up"] == true {
        assert!(t0.elapsed() < Duration::from_secs(2), "worker still up");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Neighbours are untouched.
    assert_eq!(http_get(addr, "/hostile/k/").await.0, 200);

    // The next request restarts it; `/data` survived.
    let (status, body) = http_get(addr, "/counter/k/").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""count":1"#), "{body}");
    let v = stats_json(addr).await;
    assert_eq!(v["workers"]["counter"]["starts"], 2, "{v}");
    assert_ne!(v["workers"]["counter"]["pid"].as_u64().unwrap(), pid);
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
    }
    std::thread::sleep(Duration::from_millis(400));
    report("sleep_survives_stop", true);
}

/// Many requests through the supervisor reuse its links to the worker
/// without errors or restarts.
#[tokio::test(flavor = "multi_thread")]
async fn worker_connection_outlives_many_requests() {
    let (_sup, addr) = supervised("worker_connection_outlives_many_requests").await;
    let tasks: Vec<_> = (0..32)
        .map(|t| {
            tokio::spawn(async move {
                let mut bad = 0;
                for i in 0..400 {
                    if http_get(addr, &format!("/counter/c{}/", (t + i) % 8)).await.0 != 200 {
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
    assert_eq!(v["workers"]["counter"]["starts"], 1, "{v}");
    assert_eq!(v["workers"]["counter"]["link_errors"], 0, "{v}");
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
        for app in ["counter", "hostile"] {
            let _ = std::fs::write(root.join(app).join("cgroup.kill"), "1");
            let _ = std::fs::remove_dir(root.join(app));
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
    let mut cfg = node_cfg("workers_are_placed_in_their_cgroups", 8, 60_000, 100);
    cfg.cgroup = Some(root.clone());
    let (sup, addr) = supervised_with(cfg).await;
    let v = stats_json(addr).await;
    for app in ["counter", "hostile"] {
        let pid = v["workers"][app]["pid"].as_u64().unwrap();
        let procs = std::fs::read_to_string(root.join(app).join("cgroup.procs")).unwrap();
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
    let mut cfg = node_cfg("worker_memory_max_kills_only_that_worker", 8, 60_000, 30_000);
    cfg.cgroup = Some(root.clone());
    cfg.apps[1].worker.memory_mb = 160;
    cfg.apps[1].limits.request_timeout_ms = 30_000;
    let (sup, addr) = supervised_with(cfg).await;
    let hostile = root.join("hostile");
    assert_eq!(std::fs::read_to_string(hostile.join("memory.max")).unwrap().trim(), (160u64 << 20).to_string());

    let pid = stats_json(addr).await["workers"]["hostile"]["pid"].as_u64().unwrap();
    let mut statuses = Vec::new();
    for i in 0..8 {
        statuses.push(http_get(addr, &format!("/hostile/m{i}/alloc?mb=30")).await.0);
    }
    assert!(cgroup_stat(&hostile, "memory.events", "oom_kill ") >= 1, "no OOM kill: {statuses:?}");
    assert_eq!(http_get(addr, "/counter/n/").await.0, 200, "the other app kept serving");
    // The next request starts a fresh worker.
    assert_eq!(http_get(addr, "/hostile/after/").await.0, 200);
    let v = stats_json(addr).await;
    assert_ne!(v["workers"]["hostile"]["pid"].as_u64().unwrap(), pid, "{v}");
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
    let mut cfg = node_cfg("worker_cpu_and_pids_limits_hold", 8, 60_000, 100);
    cfg.cgroup = Some(root.clone());
    cfg.apps[1].worker.cpus = 0.2;
    cfg.apps[1].worker.pids = 64;
    let (sup, addr) = supervised_with(cfg).await;
    let hostile = root.join("hostile");
    assert_eq!(std::fs::read_to_string(hostile.join("cpu.max")).unwrap().trim(), "20000 100000");
    assert_eq!(std::fs::read_to_string(hostile.join("pids.max")).unwrap().trim(), "64");

    let spins: Vec<_> = (0..4)
        .map(|i| tokio::spawn(async move { http_get(addr, &format!("/hostile/spin{i}/spin")).await }))
        .collect();
    for s in spins {
        s.await.unwrap();
    }
    assert!(cgroup_stat(&hostile, "cpu.stat", "nr_throttled ") > 0, "cpu.max did not throttle");
    let pids = cgroup_stat(&hostile, "pids.current", "");
    assert!((1..=64).contains(&pids), "pids.current = {pids}");
    drop(sup);
    remove_cgroup(&root).await;
}

/// A worker that cannot reach its own directories fails to start and says
/// which one, rather than running with a jail rule silently dropped.
#[tokio::test(flavor = "multi_thread")]
async fn unreachable_static_dir_fails_the_start_by_name() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skipped: needs root for a separate worker uid");
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("private-static");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut cfg = node_cfg("unreachable_static_dir", 8, 60_000, 100);
    cfg.apps[0].static_dir = Some(dir.clone());
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_nanovm"));
    let err = match nanovm::supervisor::Supervisor::new(cfg, exe, 1).await {
        Ok(_) => panic!("started with an unreachable static_dir"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("private-static is not accessible"), "{err}");
}
