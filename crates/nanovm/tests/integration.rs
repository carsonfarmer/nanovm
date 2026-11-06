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

#[path = "../support/collector.rs"]
mod collector;

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
        digest: "test".into(),
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
async fn app_storage_has_one_local_owner() {
    let cfg = node_cfg("app_storage_has_one_local_owner", 2);
    let app = app("counter");
    let wasm = guest("counter");
    let first = Node::new(cfg.clone(), &app, &wasm, None).unwrap();
    let held = cfg.app_dirs(&app.name)[1].join("held");
    std::fs::create_dir_all(&held).unwrap();
    std::fs::write(held.join("live"), b"untouched").unwrap();
    let reject = || {
        let result = Node::new(cfg.clone(), &app, &wasm, None);
        assert!(result.is_err(), "two nodes admitted to the same app storage");
        assert!(result.err().unwrap().to_string().contains("app storage is already owned"));
        assert_eq!(std::fs::read(held.join("live")).unwrap(), b"untouched", "competing admission cleaned live scratch");
    };
    reject();
    let mut other = app.clone();
    other.name = "other".into();
    drop(Node::new(cfg.clone(), &other, &wasm, None).unwrap());
    let owner = first.app.clone();
    first.shutdown().await;
    reject();
    drop(first);
    reject();
    drop(owner);
    assert!(Node::new(cfg.clone(), &app, b"invalid component", None).is_err());
    let reopened = Node::new(cfg, &app, &wasm, None).unwrap();
    assert_eq!(get(&reopened, "object", "/").await.status, 200);
    reopened.shutdown().await;
}

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

/// `hostile` with room for heavy file work: writes and creates run inline
/// and count against the CPU budget, which on a loaded host (the full suite)
/// can pass 100 ms. These tests are about quotas, not CPU.
fn filler() -> AppConfig {
    let mut hostile = app("hostile");
    hostile.limits.cpu_ms = 1_000;
    hostile
}

#[tokio::test(flavor = "multi_thread")]
async fn file_count_limit() {
    let node = node_with(node_cfg("file_count_limit", 8), filler());
    let r = get(&node, "f", "/files?mount=tmp").await;
    assert_eq!(r.status, 507, "{}", r.body);
    assert_eq!(r.json()["created"], 200);
    assert_eq!(r.json()["error"], "quota");
}

#[tokio::test(flavor = "multi_thread")]
async fn disk_quota_and_tmp_wipe() {
    let node = node_with(node_cfg("disk_quota_and_tmp_wipe", 8), filler());
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
    // and the old one's scratch directory, which it left full, is gone from
    // the host: the one there holds only the new instance's file.
    let scratch = node.cfg.data_dir.join("tmp/hostile");
    let dirs = entries(&scratch);
    assert_eq!(dirs.len(), 1, "{dirs:?}");
    assert_eq!(entries(&scratch.join(&dirs[0])), ["a"]);
}

/// The names in directory `dir`.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    names
}

/// An instance that leaves its `/tmp` empty hands the scratch directory to
/// the next instance, of whichever object (making and removing one per
/// instance cost more than the rest of a cold start's file work); one that
/// leaves anything there has its directory removed, so every instance
/// starts with an empty `/tmp`.
#[tokio::test(flavor = "multi_thread")]
async fn only_empty_scratch_is_reused() {
    let node = node_with(node_cfg("only_empty_scratch_is_reused", 8), filler());
    let scratch = node.cfg.data_dir.join("tmp/hostile");
    assert_eq!(get(&node, "t", "/files?mount=tmp&n=2").await.status, 200);
    assert_eq!(get(&node, "t", "/clean?mount=tmp&n=2").await.json()["removed"], 2);
    let first = entries(&scratch);
    assert_eq!(first.len(), 1, "{first:?}");
    // The reply to a trap comes before the instance is gone.
    let gone = || node.metrics.stores_live.load(Relaxed) == 0;
    assert_eq!(get(&node, "t", "/panic").await.status, 500);
    eventually("t's instance gone", gone).await;
    assert_eq!(get(&node, "u", "/files?mount=tmp&n=1").await.status, 200);
    assert_eq!(entries(&scratch), first, "u's instance reuses t's directory");
    assert_eq!(entries(&scratch.join(&first[0])), ["f0"]);
    // u leaves `f0` behind, so its directory is removed, not handed on.
    assert_eq!(get(&node, "u", "/panic").await.status, 500);
    eventually("u's instance gone", gone).await;
    assert_eq!(get(&node, "v", "/").await.status, 200);
    let now = entries(&scratch);
    assert!(now.len() == 1 && now != first, "{now:?}");
    assert!(entries(&scratch.join(&now[0])).is_empty());
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
    let node = node_with(node_cfg("data_quota_survives_restart", 8), filler());
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
    use http_body_util::BodyExt;
    use prost::Message;
    use opentelemetry_proto::tonic::{collector::trace::v1::ExportTraceServiceRequest, common::v1::any_value::Value};
    let spans = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = node_cfg("egress_allowed_request_round_trips", 8);
    cfg.telemetry_endpoint = format!("http://{}", sink.local_addr().unwrap());
    let captured = spans.clone();
    let leaked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let leak = leaked.clone();
    let collector = tokio::spawn(nanovm::server::serve_http1(sink, move |req| {
        let captured = captured.clone();
        let leak = leak.clone();
        async move {
            let trace = req.uri().path() == "/v1/traces";
            let body = req.into_body().collect().await.unwrap().to_bytes();
            if body.windows(7).any(|s| s == b"PRIVATE") { leak.store(true, Relaxed); }
            if trace {
                captured.lock().extend(ExportTraceServiceRequest::decode(body).unwrap().resource_spans
                    .into_iter().flat_map(|r| r.scope_spans).flat_map(|s| s.spans));
            }
            nanovm::server::ok_json(serde_json::json!({}))
        }
    }));
    // In dev mode an IP-literal allowlist entry may be private; this
    // exercises the whole send path.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let contexts = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let received = contexts.clone();
    let server = tokio::spawn(nanovm::server::serve_http1(listener, move |req| {
        let received = received.clone();
        async move {
            if req.uri().path() == "/stall" { return std::future::pending().await; }
            received.lock().push(req.headers().get("traceparent").and_then(|v| v.to_str().ok()).unwrap_or("").to_owned());
            let status = req.uri().path().trim_start_matches('/').parse::<u16>().unwrap();
            nanovm::object::error_response(status, "PRIVATE", "hello")
        }
    }));
    let mut hostile = app("hostile");
    hostile.limits.egress.push(format!("127.0.0.1:{port}"));
    hostile.limits.request_timeout_ms = 300;
    let node = node_with(cfg, hostile);
    for status in [201, 404, 503] {
        let r = get(&node, "n", &format!("/fetch?url=http://127.0.0.1:{port}/{status}?secret=PRIVATE")).await;
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(r.json()["status"], status);
    }
    let denied = get(&node, "n", "/fetch?url=http://127.0.0.1:1/denied?secret=PRIVATE").await;
    assert!(denied.body.contains("HttpRequestDenied"));
    let timeout = get(&node, "n", &format!("/fetch?url=http://127.0.0.1:{port}/stall")).await;
    assert_eq!(timeout.status, 504);
    node.shutdown().await;
    let telemetry = node.telemetry.clone();
    tokio::task::spawn_blocking(move || telemetry.flush().unwrap()).await.unwrap();
    let spans = spans.lock();
    let attr = |s: &opentelemetry_proto::tonic::trace::v1::Span, key: &str| {
        s.attributes.iter().find(|a| a.key == key).and_then(|a| a.value.as_ref()).and_then(|v| v.value.clone())
    };
    let clients: Vec<_> = spans.iter().filter(|s| s.kind == 3).collect();
    assert_eq!(clients.len(), 5, "every external call, including denial and cancellation, has a CLIENT span");
    for (i, status) in [201, 404, 503].into_iter().enumerate() {
        let client = clients.iter().find(|s| attr(s, "url.path") == Some(Value::StringValue(format!("/{status}")))).unwrap();
        let parent = spans.iter().find(|s| s.span_id == client.parent_span_id).unwrap();
        assert_eq!(parent.trace_id, client.trace_id);
        assert_eq!(parent.status.as_ref().map_or(0, |s| s.code), 0, "handled dependency failures don't fail the caller");
        assert_eq!(attr(client, "http.response.status_code"), Some(Value::IntValue(status)));
        assert_eq!(attr(client, "server.address"), Some(Value::StringValue("127.0.0.1".into())));
        assert_eq!(client.status.as_ref().map_or(0, |s| s.code), if status >= 400 { 2 } else { 0 });
        assert_eq!(attr(client, "error.type"), if status >= 400 { Some(Value::StringValue(status.to_string())) } else { None });
        let trace = opentelemetry::trace::TraceId::from_bytes(client.trace_id.clone().try_into().unwrap());
        let span = opentelemetry::trace::SpanId::from_bytes(client.span_id.clone().try_into().unwrap());
        assert_eq!(contexts.lock()[i], format!("00-{trace}-{span}-01"));
    }
    let denied = clients.iter().find(|s| attr(s, "url.path") == Some(Value::StringValue("/denied".into()))).unwrap();
    assert_eq!(denied.status.as_ref().unwrap().code, 2);
    let cancelled = clients.iter().find(|s| attr(s, "url.path") == Some(Value::StringValue("/stall".into()))).unwrap();
    assert_eq!(attr(cancelled, "error.type"), Some(Value::StringValue("cancelled".into())));
    assert_eq!(cancelled.status.as_ref().unwrap().code, 2);
    let parent = spans.iter().find(|s| s.span_id == cancelled.parent_span_id).unwrap();
    assert_eq!(parent.trace_id, cancelled.trace_id);
    assert_eq!(attr(parent, "error.type"), Some(Value::StringValue("timeout".into())));
    assert!(!leaked.load(Relaxed), "query secrets must not appear in spans or runtime logs");
    server.abort();
    collector.abort();
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
        "GET /_nanovm/ready HTTP/1.1\r\nHost: worker\r\nConnection: close\r\n\r\n",
        b"",
    ).await;
    assert_eq!(status, 204, "{body}");
    assert!(body.is_empty(), "readiness must not return a stats snapshot: {body}");
    assert_eq!(node.live_objects(), 0);

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

    for path in ["/_nanovm/stats", "/_nanovm/events"] {
        assert_eq!(http_get(addr, path).await.0, 404);
    }

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

type Capture = (std::sync::Weak<nanovm::supervisor::Supervisor>, Arc<parking_lot::Mutex<collector::Collector>>);
fn captures() -> &'static parking_lot::Mutex<std::collections::HashMap<std::net::SocketAddr, Capture>> {
    static CAPTURES: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashMap<std::net::SocketAddr, Capture>>> = std::sync::OnceLock::new();
    CAPTURES.get_or_init(Default::default)
}

async fn supervised_with(mut cfg: NodeConfig) -> (Arc<nanovm::supervisor::Supervisor>, std::net::SocketAddr) {
    let captured = Arc::new(parking_lot::Mutex::new(collector::Collector::default()));
    if cfg.telemetry_endpoint == NodeConfig::default().telemetry_endpoint {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        cfg.telemetry_endpoint = format!("http://{}", listener.local_addr().unwrap());
        let captured = captured.clone();
        tokio::spawn(nanovm::server::serve_http1(listener, move |req| {
            let captured = captured.clone();
            async move {
                use http_body_util::BodyExt;
                let path = req.uri().path().to_owned();
                let body = req.into_body().collect().await.unwrap().to_bytes();
                captured.lock().ingest(&path, &body).unwrap();
                http::Response::new(http_body_util::Full::new(bytes::Bytes::new()).map_err(|e| match e {}).boxed_unsync())
            }
        }));
    }
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_nanovm"));
    let sup = nanovm::supervisor::Supervisor::new(cfg, exe, 1).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    captures().lock().retain(|_, (sup, _)| sup.strong_count() != 0);
    captures().lock().insert(addr, (Arc::downgrade(&sup), captured));
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
    publish_wasm(repo, app, guest(&app.name)).await
}

async fn publish_wasm(repo: &str, app: &AppConfig, wasm: Vec<u8>) -> String {
    publish_version(repo, app, wasm, "1.0.0").await
}

async fn publish_version(repo: &str, app: &AppConfig, wasm: Vec<u8>, version: &str) -> String {
    use nanovm::registry::{APP_TYPE, CONFIG_TYPE, VERSION, WASM_TYPE};
    use oci_client::client::{ClientConfig, ClientProtocol, Config, ImageLayer};
    let layer = ImageLayer::new(wasm, WASM_TYPE.into(), None);
    let spec = serde_json::json!({ "limits": app.limits, "env": app.env, "config": app.config });
    let config = Config::new(serde_json::to_vec(&spec).unwrap(), CONFIG_TYPE.into(), None);
    let mut manifest = oci_client::manifest::OciImageManifest::build(
        std::slice::from_ref(&layer),
        &config,
        Some([(VERSION.to_string(), version.to_string())].into()),
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
    let (sup, captured) = captures().lock()[&addr].clone();
    let sup = sup.upgrade().unwrap();
    let started = Instant::now();
    // A killed worker's link can close before its process watcher runs.
    while let Err(e) = sup.flush_telemetry().await {
        assert!(started.elapsed() < Duration::from_secs(2), "telemetry flush failed: {e:#}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    captured.lock().snapshot()
}

#[tokio::test(flavor = "multi_thread")]
async fn supervised_workers_contain_attacks() {
    let (_sup, addr) = supervised("supervised_workers_contain_attacks").await;
    assert_eq!(http_get(addr, "/_nanovm/ready").await, (204, String::new()));
    for path in ["/_nanovm/stats", "/_nanovm/events"] {
        assert_eq!(http_get(addr, path).await.0, 404);
    }
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
    assert!(v["rss_mb"].as_f64().unwrap() > 0.0, "{v}");
    assert!(v["uptime_ms"].as_f64().unwrap() > 0.0, "{v}");
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
    let captured = captures().lock()[&addr].1.clone();
    assert!(captured.lock().logs.iter().any(|log| log["app"] == worker(&hostile) && log["kind"] == "egress-denied"));
}

#[tokio::test(flavor = "multi_thread")]
async fn killed_worker_is_restarted_and_keeps_its_volumes() {
    let cfg = node_cfg("killed_worker_is_restarted", 8);
    let (_sup, addr) = supervised_with(cfg.clone()).await;
    let (counter, hostile) = (publish("counter", &app("counter")).await, publish("hostile", &app("hostile")).await);
    assert_eq!(app_get(addr, &hostile, "/k/").await.0, 200);
    let incr = format!("POST /k/incr HTTP/1.1\r\nHost: x\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n", names(&counter));
    assert_eq!(raw(addr, &incr, b"").await.0, 200);

    let pid = stats_json(addr).await["workers"][worker(&counter).as_str()]["pid"].as_u64().unwrap();
    let mut competing = app("counter");
    competing.name = worker(&counter);
    let error = Node::new(cfg, &competing, &guest("counter"), None).err().expect("jailed worker must own its storage");
    assert!(error.to_string().contains("app storage is already owned"), "{error:#}");
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
    let (sup, addr) = supervised("worker_connection_outlives_many_requests").await;
    let counter = publish("counter", &app("counter")).await;
    // A worker belongs to the supervisor's control runtime, not whichever
    // front-end runtime happened to receive the request that started it.
    let (ready, port) = tokio::sync::oneshot::channel();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let front = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            ready.send(listener.local_addr().unwrap()).unwrap();
            tokio::spawn(nanovm::server::serve_http1(listener, move |req| sup.clone().handle(req)));
            stopped.await.unwrap();
        });
    });
    assert_eq!(app_get(port.await.unwrap(), &counter, "/c0/").await.0, 200);
    let pid = stats_json(addr).await["workers"][worker(&counter).as_str()]["pid"].clone();
    stop.send(()).unwrap();
    tokio::task::spawn_blocking(move || front.join().unwrap()).await.unwrap();
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
    assert_eq!(v["workers"][worker(&counter).as_str()]["pid"], pid, "front-end shutdown must not stop its worker");
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
// Signature checks load the artifacts CI publishes to GHCR without running
// their older guest interfaces. Worker drains use locally composed guests.

const SIGNER: &str = "https://token.actions.githubusercontent.com \
    https://github.com/carsonfarmer/nanovm/.github/workflows/publish-guests.yml@refs/heads/main";

async fn drain_versions(repo: &str) -> (String, String) {
    let mut hostile = app("hostile");
    hostile.limits.cpu_ms = 500;
    hostile.limits.request_timeout_ms = 2000;
    hostile.limits.post_return_timeout_ms = 1000;
    let old = publish_version(repo, &hostile, guest("hostile"), "1.0.0").await;
    let new = publish_version(repo, &hostile, guest("hostile"), "2.0.0").await;
    assert_ne!(old, new);
    (old, new)
}

/// A drain that runs out of time kills the old worker even while a client
/// still holds one of its responses unread, so it can't run beside the next
/// version. (The rollback from the newest version to the one before is a
/// switch like any other.)
#[tokio::test(flavor = "multi_thread")]
async fn a_drain_that_runs_out_of_time_kills_the_old_worker() {
    use tokio::io::AsyncWriteExt;
    let (old, new) = drain_versions("drain-timeout").await;
    let (_sup, addr) = supervised("drain_kills_the_old_worker").await;

    // A client that reads a little of a 7 MiB answer, then stops.
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.set_recv_buffer_size(4096).unwrap();
    let mut held = sock.connect(addr).await.unwrap();
    let head = format!(
        "GET /h/big?mb=7 HTTP/1.1\r\nHost: x\r\n{}\r\n",
        names(&new)
    );
    held.write_all(head.as_bytes()).await.unwrap();
    let mut response = [0; 64];
    let n = tokio::io::AsyncReadExt::read(&mut held, &mut response).await.unwrap();
    assert!(response[..n].starts_with(b"HTTP/1.1 200"), "{}", String::from_utf8_lossy(&response[..n]));
    let pid = stats_json(addr).await["workers"][worker(&new).as_str()]["pid"].as_u64().unwrap();

    let t0 = Instant::now();
    assert_eq!(app_get(addr, &old, "/x/").await.0, 200);
    // The switch waited out the drain (hostile's 2 s + 1 s), then killed it.
    assert!(t0.elapsed() >= Duration::from_secs(3), "{:?}", t0.elapsed());
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists(), "replacement answered before old worker was reaped");
    drop(held);
    let v = stats_json(addr).await;
    assert_eq!(v["workers"][worker(&old).as_str()]["version"], "1.0.0", "{v}");
    assert_ne!(v["workers"][worker(&old).as_str()]["pid"].as_u64().unwrap(), pid);
}

/// Published versions, oldest first, as `(version, ghcr.io/…@digest)`.
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

/// A registry app loads from its signed artifact, and anything short of a
/// trusted signature over that digest at an allowed version is refused.
#[tokio::test(flavor = "multi_thread")]
async fn registry_apps_need_a_trusted_signature() {
    use nanovm::registry::{AppRef, Registry};
    let (version, app) = published("counter").await.expect("published signed counter is reachable").pop().unwrap();
    let cfg = node_cfg("registry_apps_need_a_trusted_signature", 8);
    let registry = Registry::new(cfg.data_dir.join("registry"), cfg.max_component_mb << 20, false).unwrap();
    let reference = |app: &str, trust: &str, min: Option<&str>| {
        let mut headers = http::HeaderMap::new();
        headers.insert("nanovm-app", app.parse().unwrap());
        headers.insert("nanovm-trust", trust.parse().unwrap());
        if let Some(min) = min {
            headers.insert("nanovm-min-version", min.parse().unwrap());
        }
        AppRef::from_headers(&headers).unwrap().unwrap()
    };
    let accepted = reference(&app, SIGNER, Some(&version.to_string()));
    let pulled = registry.load(&accepted).await.unwrap();
    assert_eq!(pulled.version, version);
    assert_eq!(pulled.spec.config["greeting"], "from the registry");
    assert!(pulled.wasm.starts_with(b"\0asm"));

    let wrong_subject = SIGNER.replace("publish-guests.yml", "other.yml");
    let wrong_issuer = SIGNER.replace("token.actions", "evil");
    let unsigned = app.rsplit_once('@').unwrap().0.to_string() + "@sha256:" + &"0".repeat(64);
    let too_new = format!("{}.0.0", version.major + 1);
    // The cached artifact, under another policy, is checked again (twice:
    // a refusal is not remembered as an acceptance).
    for (name, a, trust, min, why) in [
        ("subject", app.as_str(), wrong_subject.as_str(), None, "not signed by a trusted identity"),
        ("subject", app.as_str(), wrong_subject.as_str(), None, "not signed by a trusted identity"),
        ("issuer", app.as_str(), wrong_issuer.as_str(), None, "not signed by a trusted identity"),
        ("floor", app.as_str(), SIGNER, Some(too_new.as_str()), "below the minimum"),
        ("digest", unsigned.as_str(), SIGNER, None, "not found"),
    ] {
        let error = registry.load(&reference(a, trust, min)).await.err().expect("untrusted artifact is refused");
        assert!(format!("{error:#}").contains(why), "{name}: {error:#}");
    }
    // Refusals do not poison an authorized reload.
    let reloaded = registry.load(&accepted).await.unwrap();
    assert_eq!(reloaded.version, version);
    assert_eq!(reloaded.wasm, pulled.wasm);
    // Trust is required at the HTTP boundary, before any instantiation.
    let (_sup, addr) = signed("registry_trust_header").await;
    let head = format!("GET /a/ HTTP/1.1\r\nHost: x\r\nnanovm-app: {app}\r\nConnection: close\r\n\r\n");
    assert_eq!(raw(addr, &head, b"").await.0, 400);
}

/// Naming a new version switches the app: requests in flight on the old
/// worker finish, the new worker serves the same volumes.
#[tokio::test(flavor = "multi_thread")]
async fn registry_app_switches_versions_after_a_drain() {
    let (old_app, new_app) = drain_versions("drain-switch").await;
    let (_sup, addr) = supervised("registry_app_switches_versions").await;

    // Cache both versions before testing the drain: downloading or
    // compiling the replacement could otherwise outlast the sleeping request.
    let (status, body) = app_get(addr, &new_app, "/warm/").await;
    assert_eq!(status, 200, "{body}");
    // Persistence needs only a tiny file and key; quotas have their own tests.
    let expected = serde_json::json!({"file": "before-switch", "key": "before-switch"});
    let (status, body) = app_get(addr, &old_app, "/w/commit?v=before-switch").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap(), expected);
    let old_worker = worker(&old_app);
    let old_pid = stats_json(addr).await["workers"][&old_worker]["pid"].as_u64().unwrap();
    let slow = tokio::spawn(async move { app_get(addr, &old_app, "/s/sleep?ms=1500").await });
    let started = Instant::now();
    loop {
        let v = stats_json(addr).await;
        if v["apps"][&old_worker]["phases"]["handling"] == 1 {
            break;
        }
        assert!(!slow.is_finished(), "old request finished before the switch: {v}");
        assert!(started.elapsed() < Duration::from_secs(2), "old request never entered handling: {v}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let (status, body) = app_get(addr, &new_app, "/w/commit").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap(), expected);
    // Observe worker lifetime, not the scheduling order of two HTTP clients.
    assert!(!Path::new(&format!("/proc/{old_pid}")).exists(), "old worker survived the switch");
    let (slow_status, slow_body) = slow.await.unwrap();
    assert_eq!(slow_status, 200, "the old version's request finished: {slow_body}");
    let v = stats_json(addr).await;
    assert_eq!(v["workers"][worker(&new_app).as_str()]["version"], "2.0.0", "{v}");
    assert_eq!(v["workers"][worker(&new_app).as_str()]["starts"], 3, "{v}");
}

// ---- composed spin:key-value --------------------------------------------

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
    let r = get(&node, "f", "/kv?op=list").await;
    assert_eq!(r.json(), serde_json::json!({"keys":198}), "full stores remain readable: {}", r.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn kv_refuses_bad_names_and_streams_keys() {
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
    let r = get(&node, "p", "/kv?name=streamed&op=fill&size=1&n=1500").await;
    assert_eq!(r.json()["set"], 1500, "{}", r.body);
    let r = get(&node, "p", "/kv?name=streamed&op=list").await;
    assert_eq!(r.json(), serde_json::json!({"keys": 1500}), "{}", r.body);
    let r = get(&node, "p", "/kv?name=empty&op=list").await;
    assert_eq!(r.json(), serde_json::json!({"keys": 0}), "{}", r.body);
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
    // A first write, not opening the bucket, now creates its directories.
    let (_, body) = app_get(addr, &hostile, "/p/kv?op=key&len=1").await;
    assert!(body.contains(r#""set":true"#), "{body}");
    let (_, body) = app_get(addr, &hostile, "/p/kv?op=plant").await;
    assert!(body.contains("IsDirectory"), "{body}");
    let bucket = std::fs::read_dir(node_dir("kv_works_in_a_jailed_worker").join("objects").join(worker(&hostile)).join("p/.kv/default")).unwrap();
    let mut names: Vec<_> = bucket.flatten().map(|e| e.file_name()).collect();
    names.sort();
    assert_eq!(names, ["aw", "azA"], "only the seed and planted directory are there");
}

fn node_dir(test: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("nodes").join(test)
}

/// The provider bounds both writes and reads to 1 MiB per value, including
/// files written outside the KV interface.
#[tokio::test(flavor = "multi_thread")]
async fn kv_values_are_bounded() {
    let node = node("kv_bounded", "hostile", 8);
    let r = get(&node, "m", "/kv?op=fill&size=1048576&n=1").await;
    assert_eq!(r.json()["set"], 1, "{}", r.body);
    assert_eq!(get(&node, "m", "/kv?op=get").await.json()["bytes"], 1 << 20);
    let r = get(&node, "m", "/kv?op=fill&size=1048577&n=1").await;
    assert_eq!(r.json()["set"], 0, "{}", r.body);
    assert!(r.body.contains("value exceeds 1 MiB"), "{}", r.body);
    assert_eq!(get(&node, "m", "/kv?op=get").await.json()["bytes"], 1 << 20, "failed write keeps old value");

    node.shutdown().await;
    let file = node.cfg.data_dir.join("objects/hostile/m/.kv/default/azA");
    std::fs::write(file, vec![7u8; (1 << 20) + 1]).unwrap();
    let r = get(&node, "m", "/kv?op=get").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("value exceeds 1 MiB"), "{}", r.body);
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

/// Composing HTTP, KV and SQL must not reduce configured live-object capacity.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires SQLite build.sh --compose"]
async fn sqlite_composed_pool_capacity() {
    let cfg = node_cfg("sqlite_pool", 100);
    let mut app = app("counter");
    app.limits.memory_mb = 64;
    app.limits.cpu_ms = 2000;
    app.limits.request_timeout_ms = 5000;
    app.limits.idle_timeout_ms = 600_000;
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/composed.wasm")).unwrap();
    let node = Node::new(cfg, &app, &wasm, None).unwrap();
    for reused in [false, true] {
        for i in 0..100 {
            let r = get(&node, &format!("o{i}"), "/").await;
            assert_eq!(r.status, 200, "object {i}: {}", r.body);
            assert_eq!(r.json()["reused"], reused, "object {i}: {}", r.body);
        }
    }
    node.shutdown().await;
}

/// The SQLite engine is a separate component, not linked into the HTTP client.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires SQLite build.sh --compose and wac 0.12.0"]
async fn sqlite_composed_provider() {
    let cfg = node_cfg("sqlite_composed", 2);
    let mut app = app("counter");
    app.limits.memory_mb = 64;
    app.limits.cpu_ms = 2000;
    app.limits.request_timeout_ms = 5000;
    let client = std::fs::read(repo().join("target/sqlite-filesystem/wasm32-wasip2/release/sqlite_client.wasm")).unwrap();
    let why = Node::new(cfg.clone(), &app, &client, None).err().expect("uncomposed SQL must not link");
    assert!(format!("{why:#}").contains("spin:sqlite/sqlite@3.1.0"));
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/composed.wasm")).unwrap();
    let node = Node::new(cfg.clone(), &app, &wasm, None).unwrap();
    let expected = |rows: &[&str], marker: &str, reused: bool| {
        serde_json::json!({"rows": rows, "file": marker, "key": marker, "reused": reused})
    };
    let first = get(&node, "a", "/?v=first").await;
    assert_eq!(first.status, 200, "{:?}: {}", first.error, first.body);
    assert_eq!(first.json(), expected(&["first"], "first", false));
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first"], "first", true));
    let limits = get(&node, "a", "/limits").await;
    assert_eq!(limits.status, 200, "{}", limits.body);
    assert_eq!(limits.json(), expected(&["first"], "first", true));
    assert_eq!(get(&node, "a", "/nohooks").await.json(), expected(&["first"], "first", true));
    for then in ["error", "trap"] {
        let fail = get(&node, "a", &format!("/?v=discard&then={then}")).await;
        assert_eq!(fail.error.as_deref(), Some(if then == "error" { "guest-error" } else { "trap" }), "{}", fail.body);
        if then == "trap" {
            // Simulate an unwriteable database during host recovery. Neither
            // a failed startup nor its retry may discard the remaining undo.
            let base = node.cfg.app_dirs(&app.name)[0].clone();
            let db = base.join("a/composed.db");
            let parked = base.join("parked.db");
            std::fs::rename(&db, &parked).unwrap();
            std::fs::create_dir(&db).unwrap();
            let log = base.join("~journal/a/log");
            for _ in 0..2 {
                let blocked = get(&node, "a", "/").await;
                assert!(blocked.status >= 500, "{}", blocked.body);
                assert!(std::fs::metadata(&log).unwrap().len() > 0, "keep undo for retry");
            }
            std::fs::remove_dir(&db).unwrap();
            std::fs::rename(parked, db).unwrap();
        }
        assert_eq!(get(&node, "a", "/").await.json(), expected(&["first"], "first", false));
    }
    let denials = node.metrics.fs_denied.load(Relaxed);
    let full = get(&node, "a", "/full?v=discard-full").await;
    assert_eq!(full.error.as_deref(), Some("trap"), "{}", full.body);
    assert!(full.body.contains("SQLite storage failure"));
    assert!(node.metrics.fs_denied.load(Relaxed) > denials);
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first"], "first", false));
    let limited = get(&node, "a", "/write-result-limit?v=discard-limit").await;
    assert_eq!(limited.error.as_deref(), Some("trap"), "{}", limited.body);
    assert!(limited.body.contains("SQLite write result limit"));
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first"], "first", false));
    let invalid = get(&node, "a", "/write-invalid-text?v=discard-text").await;
    assert_eq!(invalid.error.as_deref(), Some("trap"), "{}", invalid.body);
    assert!(invalid.body.contains("SQLite write result conversion"));
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first"], "first", false));
    let response = get(&node, "a", "/invalid-text").await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.json(), expected(&["first"], "first", true));
    let killed = get(&node, "a", "/cpu?v=discard-cpu").await;
    assert_eq!(killed.error.as_deref(), Some("cpu"), "{}", killed.body);
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first"], "first", false));
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first"], "first", true));
    let response = get(&node, "a", "/constraint?v=second&then=status").await;
    assert_eq!(response.status, 507);
    assert_eq!(response.json(), expected(&["first", "second"], "second", true));
    assert_eq!(get(&node, "a", "/close").await.json(), expected(&["first", "second"], "second", true));
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first", "second"], "second", false));
    assert_eq!(get(&node, "b", "/").await.json(), expected(&[], "", false));
    node.shutdown().await;
    drop(node);
    let node = Node::new(cfg, &app, &wasm, None).unwrap();
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first", "second"], "second", false));
    node.shutdown().await;
    println!("composed Spin SQLite: typed values, one connection, policy denial, drop/reopen, isolation, mixed rollback and restart passed");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires SQLite build.sh --compose"]
async fn sqlite_composed_recovery_cuts() {
    let cfg = node_cfg("sqlite_composed_cuts", 2);
    let mut app = app("counter");
    app.limits.memory_mb = 64;
    app.limits.cpu_ms = 5000;
    app.limits.request_timeout_ms = 10_000;
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/composed-faults.wasm")).unwrap();
    let node = Node::new(cfg, &app, &wasm, None).unwrap();
    let overlap = get(&node, "overlap", "/overlap").await;
    assert_eq!(overlap.status, 200, "{}", overlap.body);
    assert_eq!(overlap.json()["scheduling"], "serialized");
    assert_eq!(get(&node, "timeout", "/?v=before").await.status, 200);
    let timed_out = get(&node, "timeout", "/timeout?v=discard-timeout").await;
    assert_eq!(timed_out.error.as_deref(), Some("timeout"), "{}", timed_out.body);
    let entered = node.cfg.app_dirs(&app.name)[0].join("timeout/sql-wait");
    assert_eq!(std::fs::read(&entered).unwrap(), b"entered", "must reach SQLite's blocking wait");
    let recovered = get(&node, "timeout", "/").await;
    assert_eq!(recovered.json(), serde_json::json!({"rows":["before"],"file":"before","key":"before","reused":false}));
    assert!(!entered.exists(), "the interrupted SQL function's write must roll back too");
    let expected = |changed: bool| {
        let mut rows: Vec<_> = (1..=if changed {12} else {16}).map(|i| {
            serde_json::json!([i, i*3 + if changed {7} else {0}, if changed {8192} else {1024}])
        }).collect();
        if changed { rows.push(serde_json::json!([17,51,3072])); }
        rows
    };
    assert_eq!(get(&node, "control", "/cuts?op=init").await.status, 200);
    let control = get(&node, "control", "/cuts?op=mutate").await;
    assert_eq!(control.status, 200, "{}", control.body);
    assert_eq!(control.json()["rows"], serde_json::json!(expected(true)));
    assert_eq!(control.json()["file"], "after");
    assert_eq!(control.json()["key"], "after");
    let steps = control.json()["steps"].as_u64().unwrap();
    assert!(steps > 0 && steps < 1000);
    for cut in 1..=steps {
        let object = format!("cut-{cut}");
        assert_eq!(get(&node, &object, "/cuts?op=init").await.status, 200);
        let failed = get(&node, &object, &format!("/cuts?op=mutate&cut={cut}")).await;
        assert_eq!(failed.error.as_deref(), Some("trap"), "cut {cut}: {}", failed.body);
        assert!(failed.body.contains(&format!("SQLite VFS cut {cut} after")), "{}", failed.body);
        let recovered = get(&node, &object, "/cuts").await;
        assert_eq!(recovered.status, 200, "{}", recovered.body);
        assert_eq!(recovered.json()["rows"], serde_json::json!(expected(false)), "cut {cut}");
        assert_eq!(recovered.json()["file"], "before");
        assert_eq!(recovered.json()["key"], "before");
    }
    println!("composed provider: {steps} VFS cuts recovered exact SQL/file/KV state; concurrent calls serialized");
    node.shutdown().await;
}

/// Opt-in experiment: build both fixtures with experiments/sqlite-filesystem/build.sh.
/// No stock P2 filesystem import may bypass nanovm's journal to make SQLite work.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the separately built SQLite experiment (WASI SDK)"]
async fn sqlite_over_the_existing_filesystem() {
    let fixture = |name: &str| std::fs::read(repo().join("target/sqlite-filesystem").join(name)).unwrap();
    let cfg = node_cfg("sqlite_filesystem", 2);
    let mut app = app("counter");
    app.limits.cpu_ms = 2000;
    app.limits.request_timeout_ms = 5000;
    let err = Node::new(cfg.clone(), &app, &fixture("stock.wasm"), None)
        .err()
        .expect("P2 must be refused");
    let why = format!("{err:#}");
    assert!(
        why.contains("wasi:filesystem/") && why.contains("not permitted"),
        "{why}"
    );
    println!("stock SQLite rejected: {why}");
    let wasm = fixture("p3.wasm");
    let node = Node::new(cfg.clone(), &app, &wasm, None).unwrap();
    let expected = |rows: &[&str], marker: &str| {
        serde_json::json!({
            "rows": rows, "file": marker, "key": marker, "integrity": "ok", "blob_bytes": 0
        })
    };
    let r = get(&node, "a", "/?v=first").await;
    assert_eq!(r.status, 200, "{:?}: {}", r.error, r.body);
    assert_eq!(r.json(), expected(&["first"], "first"));
    for then in ["error", "panic", "spin", "sql", "sleep"] {
        let r = get(&node, "a", &format!("/?v=discard-{then}&then={then}")).await;
        assert_eq!(
            r.status,
            if then == "sleep" { 504 } else { 500 },
            "{then}: {:?}: {}",
            r.error,
            r.body
        );
        assert_eq!(
            r.error.as_deref(),
            Some(match then {
                "error" => "guest-error",
                "panic" => "trap",
                "spin" | "sql" => "cpu",
                "sleep" => "timeout",
                _ => unreachable!(),
            })
        );
        let read = get(&node, "a", "/").await;
        assert_eq!(read.json(), expected(&["first"], "first"), "after {then}");
        println!("{then}: SQL, file and KV rolled back; integrity_check=ok");
    }
    let denials = node.metrics.fs_denied.load(Relaxed);
    let r = get(&node, "a", "/full?v=discard-full").await;
    assert_eq!(r.status, 500, "{}", r.body);
    assert!(node.metrics.fs_denied.load(Relaxed) > denials, "the host must enforce its disk quota");
    assert!(
        r.body.contains("sqlite:"),
        "expected SQLite error, not unrelated kill: {}",
        r.body
    );
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first"], "first"));
    println!("quota refusal: SQL, file and KV rolled back; integrity_check=ok");
    let r = get(&node, "a", "/constraint?v=second&then=status").await;
    assert_eq!(r.status, 507, "{}", r.body);
    assert_eq!(r.json(), expected(&["first", "second"], "second"));
    assert_eq!(
        get(&node, "a", "/transaction").await.json(),
        expected(&["first", "second"], "second")
    );
    let database = cfg.app_dirs(&app.name)[0].join("a/probe.db");
    let before = std::fs::metadata(&database).unwrap().len();
    let mut grown = expected(&["first", "second"], "second");
    grown["blob_bytes"] = 262144.into();
    assert_eq!(get(&node, "a", "/grow").await.json(), grown);
    let large = std::fs::metadata(&database).unwrap().len();
    assert!(large > before, "the blob must grow the file, not just SQLite's cache");
    assert_eq!(
        get(&node, "a", "/vacuum").await.json(),
        expected(&["first", "second"], "second")
    );
    let compact = std::fs::metadata(&database).unwrap().len();
    assert!(compact < large);
    println!("database growth and VACUUM: {before} -> {large} -> {compact} bytes; inner SQL rollback passed");
    assert_eq!(get(&node, "b", "/").await.json(), expected(&[], ""));
    settle(&node).await;
    assert_eq!(get(&node, "c", "/?v=other").await.status, 200);
    settle(&node).await;
    assert!(node.metrics.evicted_lru.load(Relaxed) > 0);
    assert_eq!(
        get(&node, "a", "/").await.json(),
        expected(&["first", "second"], "second")
    );
    node.shutdown().await;
    drop(node);
    let restarted = Node::new(cfg, &app, &wasm, None).unwrap();
    assert_eq!(
        get(&restarted, "a", "/").await.json(),
        expected(&["first", "second"], "second")
    );
    println!("handled constraint + HTTP 507 committed; VACUUM, isolation, eviction and node restart passed");
    restarted.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the separately built SQLite experiment (WASI SDK)"]
async fn sqlite_survives_a_jailed_worker_crash() {
    for fixture in ["p3", "composed"] {
        let mut cfg = node_cfg(&format!("sqlite_worker_crash_{fixture}"), 8);
        let mut app = app("counter");
        app.limits.memory_mb = 64;
        app.limits.cpu_ms = 2000;
        app.limits.request_timeout_ms = 30_000;
        // The supervisor clamps tenant budgets; this is a crash-recovery
        // test, not a test of the default 200 ms CPU ceiling.
        cfg.limits.cpu_ms = app.limits.cpu_ms;
        let (_sup, addr) = supervised_with(cfg.clone()).await;
        let wasm = std::fs::read(repo().join(format!("target/sqlite-filesystem/{fixture}.wasm"))).unwrap();
        let image = publish_wasm("sqlite-probe", &app, wasm).await;
        let (status, before) = app_get(addr, &image, "/a/?v=durable").await;
        assert_eq!(status, 200, "{before}");
        let name = worker(&image);
        let pid = stats_json(addr).await["workers"][&name]["pid"].as_u64().unwrap() as i32;
        let pending = tokio::spawn({
            let image = image.clone();
            async move { app_get(addr, &image, "/a/?v=uncommitted&then=sleep").await }
        });
        // Both clients write markers after SQLite's INSERT completes, before response commit.
        let marker = cfg.app_dirs(&name)[0].join("a/marker");
        let key = cfg.app_dirs(&name)[0].join("a/.kv/default/bWFya2Vy");
        eventually("SQLite, file and KV writes before SIGKILL", || {
            std::fs::read(&marker).is_ok_and(|v| v == b"uncommitted")
                && std::fs::read(&key).is_ok_and(|v| v == b"uncommitted")
        }).await;
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        let _ = pending.await;
        let start = Instant::now();
        while stats_json(addr).await["workers"][&name]["up"] == true {
            assert!(start.elapsed() < Duration::from_secs(5), "worker still up");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (status, after) = app_get(addr, &image, "/a/").await;
        assert_eq!(status, 200, "{after}");
        let mut expected = serde_json::json!({"rows": ["durable"], "file": "durable", "key": "durable"});
        if fixture == "p3" {
            expected["integrity"] = "ok".into();
            expected["blob_bytes"] = 0.into();
        } else {
            expected["reused"] = false.into();
        }
        assert_eq!(serde_json::from_str::<serde_json::Value>(&after).unwrap(), expected);
        println!("{fixture}: jailed worker SIGKILL after SQLite commit, before response: SQL/file/KV recovered together; integrity_check=ok");
    }
}

/// Faults at every VFS mutation/sync boundary of a multi-database transaction.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the separately built SQLite experiment (WASI SDK)"]
async fn sqlite_journal_pressure_matrix() {
    let cfg = node_cfg("sqlite_pressure", 8);
    let mut app = app("counter");
    app.limits.cpu_ms = 5000;
    app.limits.request_timeout_ms = 15_000;
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/p3.wasm")).unwrap();
    let node = Node::new(cfg, &app, &wasm, None).unwrap();
    let expected = |changed: bool| {
        let rows: Vec<_> = (1..=16).map(|id| serde_json::json!([
            id, id*3 + if changed { 7 } else { 0 }, if changed { 2048 } else { 1024 }
        ])).collect();
        let marker = if changed { "after" } else { "before" };
        serde_json::json!({"rows": rows, "tally": if changed { 40 } else { 29 },
            "file": marker, "key": marker, "integrity": "ok", "aux_integrity": "ok"})
    };
    for mode in ["DELETE", "TRUNCATE", "PERSIST", "MEMORY", "WAL"] {
        let exclusive = if mode == "WAL" { "&exclusive=1" } else { "" };
        let url = |op: &str, cut: u64| format!("/pressure?mode={mode}&cache=1&op={op}&cut={cut}{exclusive}");
        let control = format!("{mode}-control");
        let init = get(&node, &control, &url("init", 0)).await;
        assert_eq!(init.status, 200, "{mode}: {}", init.body);
        assert_eq!(init.json()["state"], expected(false));
        let r = get(&node, &control, &url("mutate", 0)).await;
        assert_eq!(r.status, 200, "{mode}: {}", r.body);
        let result = r.json();
        assert_eq!(result["mode"], mode.to_lowercase());
        assert_eq!(result["aux_mode"], mode.to_lowercase());
        assert_eq!(result["state"], expected(true));
        assert_eq!(result["reused"], true, "must exercise a cached connection");
        let steps = result["steps"].as_u64().unwrap();
        assert!(steps > 0 && steps < 512, "unexpected workload size: {result}");
        assert_eq!(get(&node, &control, &url("read", 0)).await.json()["state"], expected(true));
        for cut in 1..=steps {
            let object = format!("{mode}-{cut}");
            let init = get(&node, &object, &url("init", 0)).await;
            assert_eq!(init.status, 200, "{mode} cut {cut}: {}", init.body);
            let failed = get(&node, &object, &url("mutate", cut)).await;
            assert_eq!(failed.error.as_deref(), Some("trap"), "{mode} cut {cut}: {}", failed.body);
            assert!(failed.body.contains(&format!("SQLite VFS cut {cut} after")), "{}", failed.body);
            let recovered = get(&node, &object, &url("read", 0)).await;
            assert_eq!(recovered.status, 200, "{mode} cut {cut}: {}", recovered.body);
            assert_eq!(recovered.json()["state"], expected(false), "{mode} cut {cut}");
            assert_eq!(recovered.json()["reused"], false, "failed instance must lose its SQLite cache");
        }
        println!("{mode}: {steps} VFS cuts recovered exact rows in both databases + file/KV; {} SQLite write bytes", result["write_bytes"]);
    }
    assert_eq!(get(&node, "wal", "/pressure?op=init").await.status, 200);
    let wal = get(&node, "wal", "/pressure?mode=WAL&op=mutate").await;
    assert_eq!(wal.error.as_deref(), Some("guest-error"), "{}", wal.body);
    assert!(wal.body.contains("disk I/O error"), "{}", wal.body);
    assert_eq!(get(&node, "wal", "/pressure").await.json()["state"], expected(false));
    println!("normal-locking WAL: disk I/O error with this VFS; previous state recovered");

    let init = get(&node, "churn", "/pressure?op=init&cache=1").await;
    assert_eq!(init.status, 200, "{}", init.body);
    let denials = node.metrics.fs_denied.load(Relaxed);
    let failed = get(&node, "churn", "/pressure?op=churn&n=1000&cache=1&full=insert").await;
    assert_eq!(failed.error.as_deref(), Some("guest-error"), "{}", failed.body);
    assert!(node.metrics.fs_denied.load(Relaxed) > denials);
    let recovered = get(&node, "churn", "/pressure?cache=1").await;
    assert_eq!(recovered.status, 200, "{}", recovered.body);
    assert_eq!(recovered.json()["state"], expected(false));
    println!("an oversized INSERT after repeated updates fails the volume quota; full recovery verified");
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the separately built SQLite experiment (WASI SDK)"]
async fn sqlite_handled_storage_exhaustion() {
    let cfg = node_cfg("sqlite_handled_exhaustion", 8);
    let mut app = app("counter");
    app.limits.cpu_ms = 5000;
    app.limits.request_timeout_ms = 15_000;
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/p3.wasm")).unwrap();
    let node = Node::new(cfg, &app, &wasm, None).unwrap();
    for object in ["insert", "bulk"] {
        assert_eq!(get(&node, object, "/pressure?op=init&cache=1").await.status, 200);
        let denials = node.metrics.fs_denied.load(Relaxed);
        let handled = get(&node, object, &format!("/pressure?op=churn&n=1000&full={object}&cache=1&handle=1")).await;
        assert_eq!(handled.status, 200, "handled exhaustion: {}", handled.body);
        assert!(node.metrics.fs_denied.load(Relaxed) > denials);
        let handled = handled.json();
        assert!(handled["handled"].is_string(), "{handled}");
        let completed = handled["completed"].as_i64().unwrap();
        assert_eq!(completed, 1000);
        let after = get(&node, object, "/pressure").await;
        println!("handled: {handled}; next read {}: {}", after.status, after.body);
        assert_eq!(after.status, 200, "{}", after.body);
        let after = after.json();
        let rows: Vec<_> = (1..=16).map(|id| serde_json::json!([
            id, id*3 + if id == 1 { completed } else { 0 }, 1024
        ])).collect();
        assert_eq!(after["state"], serde_json::json!({"rows": rows, "tally": 29,
            "file": "after", "key": "after", "integrity": "ok", "aux_integrity": "ok"}),
            "a failed statement must not commit, including partial multi-page updates");
        println!("handled exhaustion: {completed} successful statements committed without corruption");
    }
    node.shutdown().await;
}

/// Repeated overwrites and short-lived journals must fit without raising quotas.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the separately built SQLite experiment (WASI SDK)"]
async fn sqlite_repeated_updates_remain_bounded() {
    let cfg = node_cfg("sqlite_bounded_churn", 8);
    let mut app = app("counter");
    app.limits.cpu_ms = 10_000;
    app.limits.request_timeout_ms = 60_000;
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/p3.wasm")).unwrap();
    let node = Node::new(cfg, &app, &wasm, None).unwrap();
    for mode in ["DELETE", "TRUNCATE", "PERSIST", "MEMORY", "WAL"] {
        for rollback in [false, true] {
            let object = format!("{mode}-{rollback}");
            let policy = if mode == "WAL" { "&exclusive=1&autocheckpoint=16" } else { "" };
            let prefix = format!("/pressure?mode={mode}{policy}");
            let init = get(&node, &object, &format!("{prefix}&op=init&cache=1")).await;
            assert_eq!(init.status, 200, "{}", init.body);
            let before = init.json()["state"].clone();
            let then = if rollback { "&then=error" } else { "" };
            let result = get(&node, &object, &format!("{prefix}&op=churn&n=1000{then}")).await;
            if rollback {
                assert_eq!(result.error.as_deref(), Some("guest-error"), "{}", result.body);
                assert!(result.body.contains("after 1000 completed statements"), "{}", result.body);
                let dir = node.cfg.app_dirs(&app.name)[0].join("~journal").join(&object);
                let log = std::fs::metadata(dir.join("log")).unwrap().len();
                let retained: u64 = std::fs::read_dir(&dir).unwrap().map(|entry| {
                    let entry = entry.unwrap();
                    if entry.file_name() == "log" { 0 } else { entry.metadata().unwrap().len() }
                }).sum();
                println!("{mode}: 1000 completed updates; undo log {log} bytes, retained files {retained} bytes");
                assert!(log < 256 * 1024 && retained < 256 * 1024,
                    "repeated changes must not approach the 1 MiB budget: log={log}, retained={retained}");
            } else {
                assert_eq!(result.status, 200, "{mode}: {}", result.body);
                assert_eq!(result.json()["completed"], 1000);
                assert!(result.json()["handled"].is_null());
            }
            let read = get(&node, &object, &prefix).await;
            assert_eq!(read.status, 200, "{}", read.body);
            let mut expected = before;
            if !rollback {
                expected["rows"][0][1] = 1003.into();
                expected["file"] = "after".into();
                expected["key"] = "after".into();
            }
            assert_eq!(read.json()["state"], expected, "{mode}, rollback={rollback}");
        }
    }
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the separately built SQLite experiment (WASI SDK)"]
async fn sqlite_exclusive_wal_checkpoints() {
    let cfg = node_cfg("sqlite_wal_checkpoints", 8);
    let mut app = app("counter");
    app.limits.cpu_ms = 10_000;
    app.limits.request_timeout_ms = 30_000;
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/p3.wasm")).unwrap();
    let node = Node::new(cfg, &app, &wasm, None).unwrap();
    let prefix = "/pressure?mode=WAL&exclusive=1&cache=1";
    for checkpoint in ["PASSIVE", "FULL", "RESTART", "TRUNCATE"] {
        let control = format!("{checkpoint}-control");
        assert_eq!(get(&node, &control, &format!("{prefix}&op=init")).await.status, 200);
        let changed = get(&node, &control, &format!("{prefix}&op=mutate")).await;
        assert_eq!(changed.status, 200, "{}", changed.body);
        let expected = changed.json()["state"].clone();
        let volume = node.cfg.app_dirs(&app.name)[0].join(&control);
        for db in ["pressure", "aux"] {
            assert!(std::fs::metadata(volume.join(format!("{db}.db-wal"))).unwrap().len() > 32);
            assert!(!volume.join(format!("{db}.db-shm")).exists());
        }
        let url = format!("{prefix}&op=checkpoint&checkpoint={checkpoint}");
        let done = get(&node, &control, &url).await;
        assert_eq!(done.status, 200, "{}", done.body);
        let done = done.json();
        assert_eq!(done["state"], expected);
        for result in done["checkpoints"].as_array().unwrap() {
            assert_eq!(result[0], 0, "checkpoint must not be busy");
            assert_eq!(result[1], result[2], "all WAL frames must be checkpointed");
            if checkpoint == "TRUNCATE" { assert_eq!(result[1], 0); }
            else { assert!(result[1].as_i64().unwrap() > 0); }
        }
        if checkpoint == "TRUNCATE" {
            for db in ["pressure", "aux"] {
                assert_eq!(std::fs::metadata(volume.join(format!("{db}.db-wal"))).unwrap().len(), 0);
            }
        }
        let steps = done["steps"].as_u64().unwrap();
        assert!(steps > 0 && steps < 128);
        for cut in 1..=steps {
            let object = format!("{checkpoint}-{cut}");
            assert_eq!(get(&node, &object, &format!("{prefix}&op=init")).await.status, 200);
            assert_eq!(get(&node, &object, &format!("{prefix}&op=mutate")).await.json()["state"], expected);
            let failed = get(&node, &object, &format!("{url}&cut={cut}")).await;
            assert_eq!(failed.error.as_deref(), Some("trap"), "{}", failed.body);
            assert!(failed.body.contains(&format!("SQLite VFS cut {cut} after")));
            let read = get(&node, &object, prefix).await;
            assert_eq!(read.status, 200, "{}", read.body);
            assert_eq!(read.json()["state"], expected, "{checkpoint} cut {cut}");
            assert_eq!(read.json()["reused"], false);
        }
        println!("exclusive WAL {checkpoint}: {steps} checkpoint cuts; exact SQL/file/KV state recovered");
    }
    let before = get(&node, "default-checkpoint", &format!("{prefix}&op=init")).await;
    assert_eq!(before.status, 200, "{}", before.body);
    let denials = node.metrics.fs_denied.load(Relaxed);
    let failed = get(&node, "default-checkpoint", &format!("{prefix}&op=churn&n=1000")).await;
    assert_eq!(failed.error.as_deref(), Some("guest-error"), "{}", failed.body);
    assert!(node.metrics.fs_denied.load(Relaxed) > denials);
    let wal = node.cfg.app_dirs(&app.name)[0].join("default-checkpoint/pressure.db-wal");
    println!("default WAL autocheckpoint exhausts the 1 MiB volume: WAL {} bytes", std::fs::metadata(wal).unwrap().len());
    let recovered = get(&node, "default-checkpoint", prefix).await;
    assert_eq!(recovered.status, 200, "{}", recovered.body);
    assert_eq!(recovered.json()["state"], before.json()["state"]);
    node.shutdown().await;
}

/// Manual benchmark, deliberately outside the `sqlite_` correctness-test filter.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual SQLite benchmark; requires the separately built experiment"]
async fn benchmark_database_modes() {
    let cfg = node_cfg("sqlite_benchmark", 8);
    let mut app = app("counter");
    app.limits.memory_mb = 64;
    app.limits.disk_mb = 16;
    app.limits.cpu_ms = 10_000;
    app.limits.request_timeout_ms = 30_000;
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/p3.wasm")).unwrap();
    let node = Node::new(cfg, &app, &wasm, None).unwrap();
    let variants = [("DELETE", 1000), ("WAL", 16), ("WAL", 1000)];
    let mut results = vec![];
    for round in 0..3 {
        for workload in ["point", "scan", "write", "batch", "autocommit", "blob"] {
            for cache in [true, false] {
                for j in 0..3 {
                    let (mode, pages) = variants[(j + round) % 3];
                    let object = format!("{round}-{workload}-{cache}-{mode}-{pages}");
                    let prefix = format!("/bench?mode={mode}&pages={pages}&cache={}", u8::from(cache));
                    let init = get(&node, &object, &format!("{prefix}&op=init")).await;
                    assert_eq!(init.status, 200, "{}", init.body);
                    let mut samples = vec![];
                    let (mut bytes, mut syncs, mut peak) = (0u64, 0u64, 0u64);
                    let writes = match workload { "batch" | "autocommit" => 100, "write" | "blob" => 1, _ => 0 };
                    for seq in 0..28 {
                        let start = Instant::now();
                        let response = get(&node, &object, &format!("{prefix}&op={workload}&seq={seq}")).await;
                        let us = start.elapsed().as_secs_f64() * 1_000_000.0;
                        assert_eq!(response.status, 200, "{object}: {}", response.body);
                        let body = response.json();
                        let expected = match workload { "point" => 512, "scan" => 1024*512, _ => writes };
                        assert_eq!(body["value"], expected);
                        if seq >= 3 {
                            samples.push(us);
                            bytes += body["sqlite_write_bytes"].as_u64().unwrap();
                            syncs += body["sqlite_syncs"].as_u64().unwrap();
                        }
                        let volume = node.cfg.app_dirs(&app.name)[0].join(&object);
                        let live: u64 = std::fs::read_dir(volume).unwrap()
                            .map(|e| e.unwrap().metadata().unwrap().len()).sum();
                        peak = peak.max(live);
                    }
                    // Include deferred WAL cleanup in the lifecycle cost, not in p50/p95.
                    let start = Instant::now();
                    let close = get(&node, &object, &format!("{prefix}&op=close")).await;
                    let close_us = start.elapsed().as_secs_f64()*1_000_000.0;
                    assert_eq!(close.status, 200, "{}", close.body);
                    let closed = close.json();
                    let verify = get(&node, &object, &format!("{prefix}&op=verify")).await;
                    assert_eq!(verify.status, 200, "{}", verify.body);
                    assert_eq!(verify.json()["value"], 28*writes);
                    assert_eq!(verify.json()["integrity"], "ok");
                    let result = serde_json::json!({"round": round, "workload": workload, "cached": cache,
                        "mode": mode, "checkpoint_pages": pages, "samples_us": samples,
                        "close_us": close_us, "sqlite_write_bytes": bytes, "sqlite_syncs": syncs,
                        "close_write_bytes": closed["sqlite_write_bytes"], "close_syncs": closed["sqlite_syncs"],
                        "peak_live_file_bytes": peak});
                    println!("benchmark {object}: {} us mean including close", (samples.iter().sum::<f64>()+close_us)/25.0);
                    results.push(result);
                }
            }
        }
    }
    std::fs::write(repo().join("target/sqlite-filesystem/benchmark-results.json"),
        serde_json::to_vec_pretty(&results).unwrap()).unwrap();
    node.shutdown().await;
}

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
    type Records = Arc<parking_lot::Mutex<std::collections::HashMap<String, Vec<String>>>>;

    async fn front(label: &str, mut cfg: nanovm::config::ProxyConfig, apps: &[(&str, AppConfig)]) -> (Arc<nanovm::supervisor::Supervisor>, std::net::SocketAddr, std::net::SocketAddr, Records) {
        let control = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut node_cfg = node_cfg(label, 8);
        node_cfg.publish_to = format!("http://{}/publish", control.local_addr().unwrap());
        let (sup, node) = supervised_with(node_cfg).await;
        let records = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        for (host, app) in apps {
            let published = publish(&format!("{label}-{host}"), app).await;
            records.lock().insert(format!("_nanovm.{host}"), vec![format!("app={}:latest; trust=dev dev", published.split_once('@').unwrap().0)]);
        }
        cfg.listen = "127.0.0.1:0".into();
        cfg.node = node.to_string();
        cfg.resolver = doh(records.clone()).await;
        cfg.tag_ttl_s = 0;
        cfg.dev = true;
        let proxy = nanovm::proxy::Proxy::new(cfg).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(nanovm::proxy::serve(proxy.clone(), listener));
        tokio::spawn(nanovm::proxy::serve_publish(proxy, control));
        (sup, node, addr, records)
    }

    async fn app_get(addr: std::net::SocketAddr, host: &str, path: &str) -> (u16, String) {
        raw(addr, &format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"), b"").await
    }

    async fn ended(addr: std::net::SocketAddr, host: &str, object: &str, n: usize) {
        let t0 = Instant::now();
        while app_get(addr, host, &format!("/{object}/kv/ws-ends")).await != (200, n.to_string()) {
            assert!(t0.elapsed() < Duration::from_secs(10), "{host}/{object} never ended {n} sockets");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A handshake for `path` on `app`.
    fn handshake(addr: std::net::SocketAddr, app: &str, path: &str) -> http::Request<()> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut req = format!("ws://{addr}{path}").into_client_request().unwrap();
        req.headers_mut().insert("host", app.parse().unwrap());
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

    async fn post(addr: std::net::SocketAddr, app: &str, path: &str) -> u16 {
        let head = format!("POST {path} HTTP/1.1\r\nHost: {app}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        raw(addr, &head, b"").await.0
    }

    /// An existing socket uses the new digest, retains its subscriptions,
    /// and cannot switch the node back to its OPEN version.
    #[tokio::test(flavor = "multi_thread")]
    async fn sockets_follow_current_versions() {
        let (_sup, node, addr, _) = front("socket_versions", Default::default(), &[("counter", app("counter"))]).await;
        let mut ws = open(addr, "counter", "/room/ws").await.unwrap();
        let mut changed = app("counter");
        changed.config.insert("greeting".into(), "new version".into());
        let next_version = publish("socket_versions-counter", &changed).await;
        say(&mut ws, "pub after deploy").await;
        assert_eq!(next(&mut ws).await, Message::text("after deploy"));
        let name = nanovm::registry::app_name("counter", &next_version.parse().unwrap());
        assert_eq!(stats_json(node).await["workers"][&name]["digest"], next_version.split_once('@').unwrap().1);
        assert_eq!(app_get(addr, "counter", "/room/config/greeting").await, (200, "new version".into()));
        say(&mut ws, "still new").await;
        assert_eq!(next(&mut ws).await, Message::text("echo still new"));
        assert_eq!(stats_json(node).await["workers"][&name]["starts"], 2);
    }

    /// Owners, not request hosts or repositories, scope channels and caps.
    #[tokio::test(flavor = "multi_thread")]
    async fn sockets_stay_with_their_record_owner_and_repository() {
        let cfg = nanovm::config::ProxyConfig { max_sockets_per_app: 2, ..Default::default() };
        let (_sup, _node, addr, records) = front("socket_owners", cfg, &[("counter", app("counter")), ("replacement", app("counter"))]).await;
        {
            let mut r = records.lock();
            let same = r["_nanovm.counter"].clone();
            r.insert("_nanovm.other".into(), same);
            for alias in ["alias", "alias2"] {
                r.insert(format!("_nanovm.{alias}"), vec!["CNAME _nanovm.counter".into()]);
            }
        }
        let mut a = open(addr, "counter", "/room/ws").await.unwrap();
        let mut b = open(addr, "alias", "/room/ws").await.unwrap();
        assert_eq!(refused(open(addr, "alias2", "/room/ws").await).0, 503);
        let mut other = open(addr, "other", "/room/ws").await.unwrap();
        say(&mut a, "pub shared").await;
        assert_eq!(next(&mut a).await, Message::text("shared"));
        assert_eq!(next(&mut b).await, Message::text("shared"));
        say(&mut other, "isolated").await;
        assert_eq!(next(&mut other).await, Message::text("echo isolated"));

        // A changed owner, even on the same repository, is another app.
        records.lock().insert("_nanovm.alias".into(), vec!["CNAME _nanovm.other".into()]);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        say(&mut b, "pub must not arrive").await;
        let Message::Close(Some(f)) = next(&mut b).await else { panic!("no close frame") };
        assert_eq!(f.code, CloseCode::Error);
        say(&mut other, "still isolated").await;
        assert_eq!(next(&mut other).await, Message::text("echo still isolated"));
        assert_eq!(app_get(addr, "other", "/room/kv/ws-ends").await.0, 404);

        // The same owner naming a different repository is also another app.
        let replacement = records.lock()["_nanovm.replacement"].clone();
        records.lock().insert("_nanovm.counter".into(), replacement);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        say(&mut a, "pub must not arrive either").await;
        let Message::Close(Some(f)) = next(&mut a).await else { panic!("no close frame") };
        assert_eq!(f.code, CloseCode::Error);
        assert_eq!(app_get(addr, "counter", "/room/kv/ws-ends").await.0, 404);
    }

    /// Events reach the object, which answers on the socket and publishes to
    /// the sockets of its own channel (from an event or a plain request), and
    /// learns how each socket ended unless it closed it itself. Events wait
    /// for room in a full queue, only sockets send them, and handshakes are
    /// checked.
    #[tokio::test(flavor = "multi_thread")]
    async fn websockets_reach_their_object_and_fan_out_within_it() {
        let mut counter = app("counter");
        counter.limits.max_queue = 2;
        let (_sup, _node, addr, _) = front("websockets_fan_out", Default::default(), &[("counter", counter.clone()), ("other", counter), ("hostile", app("hostile"))]).await;
        let (counter, hostile) = ("counter", "hostile");
        let (mut a, mut b) = (open(addr, &counter, "/room/ws").await.unwrap(), open(addr, &counter, "/room/ws").await.unwrap());
        let mut other = open(addr, &counter, "/elsewhere/ws").await.unwrap();
        let mut other_app = open(addr, "other", "/room/ws").await.unwrap();

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
        say(&mut other_app, "x").await;
        assert_eq!(next(&mut other_app).await, Message::text("echo x"), "another app hears no publish even with the same object and channel");

        say(&mut b, "close").await;
        let Message::Close(Some(f)) = next(&mut b).await else { panic!("no close frame") };
        assert_eq!(f.code, CloseCode::from(4000));
        a.close(None).await.unwrap();
        drop(other);
        ended(addr, counter, "room", 2).await;
        ended(addr, counter, "elsewhere", 1).await;
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
        ended(addr, counter, "busy", 8).await;
        assert_eq!(app_get(addr, &counter, "/busy/kv/ws-ends").await, (200, "8".into()));

        // An object that doesn't answer `OPEN` refuses the socket, with its
        // own status if it gave one.
        assert_eq!(refused(open(addr, &hostile, "/x/").await).0, 502);
        assert_eq!(refused(open(addr, &counter, "/room/nope").await).0, 404);
        // A client can't send events, and handshakes are checked.
        let forged = format!(
            "POST /room/ws HTTP/1.1\r\nHost: {counter}\r\nContent-Type: application/websocket-events\r\n\
            Connection-Id: 1\r\nContent-Length: 12\r\nConnection: close\r\n\r\nDISCONNECT\r\n",
        );
        assert_eq!(raw(addr, &forged, b"").await.0, 400);
        let old = format!(
            "GET /room/ws HTTP/1.1\r\nHost: {counter}\r\nUpgrade: websocket\r\nConnection: Upgrade, close\r\n\
            Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 8\r\n\r\n",
        );
        assert_eq!(raw(addr, &old, b"").await.0, 426);
        assert_eq!(app_get(addr, &counter, "/room/kv/ws-ends").await, (200, "2".into()));
    }

    /// Sockets per object, per app and per node are capped, a message is
    /// bounded by the request limit, a client that falls behind is dropped,
    /// and a worker's sockets close with it.
    #[tokio::test(flavor = "multi_thread")]
    async fn websocket_limits_hold() {
        let cfg = nanovm::config::ProxyConfig { max_sockets: 4, max_sockets_per_app: 3, max_sockets_per_object: 2,
            max_request_body_kb: 16, max_response_body_kb: 64, ..Default::default() };
        let mut counter = app("counter");
        let limits = &mut counter.limits;
        (limits.max_request_body_kb, limits.max_response_body_kb) = (16, 64);
        let (_sup, node, addr, _) = front("websocket_limits_hold", cfg, &[("counter", counter.clone()), ("chat", counter)]).await;
        let (counter, chat) = ("counter", "chat");

        let a1 = open_slow(addr, &counter, "/a/ws").await;
        let _a2 = open_slow(addr, &counter, "/a/ws").await;
        let (status, body) = refused(open(addr, &counter, "/a/ws").await);
        assert_eq!((status, body.contains("object has max_sockets")), (503, true), "{body}");
        let mut big = open(addr, &counter, "/b/ws").await.unwrap();
        let (status, body) = refused(open(addr, &counter, "/c/ws").await);
        assert_eq!((status, body.contains("app has max_sockets")), (503, true), "{body}");
        let other_app = open(addr, &chat, "/x/ws").await.unwrap();
        let (status, body) = refused(open(addr, &chat, "/y/ws").await);
        assert_eq!((status, body.contains("proxy has max_sockets")), (503, true), "{body}");
        drop(other_app);
        ended(addr, chat, "x", 1).await;

        big.send(Message::text("x".repeat(16 << 10))).await.unwrap();
        let Message::Close(Some(f)) = next(&mut big).await else { panic!("no close frame") };
        assert_eq!(f.code, CloseCode::Size);
        ended(addr, counter, "b", 1).await;

        // `a1` and `a2` never read; what is published to them piles up until
        // they are dropped.
        let msg = "y".repeat(1000);
        for i in 0.. {
            assert_eq!(post(addr, &counter, &format!("/a/publish?msg={msg}&n=50")).await, 200);
            if app_get(addr, counter, "/a/kv/ws-ends").await == (200, "2".into()) {
                break;
            }
            assert!(i < 2000, "a socket that never reads was never dropped");
        }
        assert_eq!(app_get(addr, &counter, "/a/kv/ws-ends").await, (200, "2".into()));
        drop(a1);

        // A client that gives up while its `OPEN` waits on the object still
        // has its socket ended for the object, which accepted it.
        let stats = stats_json(node).await;
        let pid = stats["workers"].as_object().unwrap().iter().find(|(name, _)| name.starts_with("counter_")).unwrap().1["pid"].as_u64().unwrap() as i32;
        unsafe { libc::kill(pid, libc::SIGSTOP) };
        let upgrade = format!(
            "GET /e/ws HTTP/1.1\r\nHost: {counter}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
            Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
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

        // A worker exit doesn't close its sockets; their next event starts it.
        let mut s = open(addr, &counter, "/d/ws").await.unwrap();
        unsafe { libc::kill(pid, libc::SIGKILL) };
        tokio::time::sleep(Duration::from_millis(100)).await;
        say(&mut s, "survived").await;
        assert_eq!(next(&mut s).await, Message::text("echo survived"));
    }
}

// ---- the proxy -------------------------------------------------------------

/// A DNS-over-HTTPS stub answering TXT queries from `records`, by name.
async fn doh(records: Arc<parking_lot::Mutex<std::collections::HashMap<String, Vec<String>>>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(nanovm::server::serve_http1(listener, move |req: http::Request<hyper::body::Incoming>| {
        let q = req.uri().query().unwrap_or("");
        let mut name = q.split('&').find_map(|kv| kv.strip_prefix("name=")).unwrap_or("").to_string();
        let mut answer = Vec::new();
        // A record `CNAME <target>` answers as resolvers do: the alias, then
        // the target's records under the target's name.
        let target = records.lock().get(&name).and_then(|r| r.first()?.strip_prefix("CNAME ").map(String::from));
        if let Some(target) = target {
            answer.push(serde_json::json!({ "name": name, "type": 5, "TTL": 1, "data": format!("{target}.") }));
            name = target;
        }
        answer.extend(records.lock().get(&name).into_iter().flatten().map(|txt| {
            serde_json::json!({ "name": format!("{name}."), "type": 16, "TTL": 1, "data": format!("\"{txt}\"") })
        }));
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
    // An alias (its record a CNAME of counter.test's) is counter.test's app.
    record("www.counter.test", &["CNAME _nanovm.counter.test"]);
    let (status, body) = get("WWW.counter.test", "/a/", "").await;
    assert!(status == 200 && body.contains(&at("counter.test", &counter)), "{body}");

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
    // Ever-new policies don't grow what the supervisor remembers.
    for i in 0..20 {
        assert_eq!(raw(addr, &with_floor(&format!("0.0.{i}")), b"").await.0, 200);
    }
    let v = stats_json(addr).await;
    let w = &v["workers"][worker(&counter).as_str()];
    assert!(w["policies"].as_u64().unwrap() <= 16 && w["starts"] == 1, "{v}");
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
/// finishes its headers is hung up on after 10 s, and
/// running out of file descriptors pauses accepting rather than ending the
/// process.
#[tokio::test(flavor = "multi_thread")]
async fn the_proxy_outlasts_idle_and_excess_connections() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("proxy_outlasts");
    std::fs::create_dir_all(&dir).unwrap();
    let port = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    // A proxy whose resolver is unreachable: every request is its 502.
    let start = |name: &str| {
        let addr = port();
        let cfg = dir.join(format!("{name}.toml"));
        let toml = format!("listen = \"{addr}\"\nresolver = \"http://127.0.0.1:1/dns-query\"\n");
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

    let (mut plain, addr) = start("plain");
    let mut slow = connect(addr).await;
    slow.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n").await.unwrap();
    let headers = tokio::spawn(async move { hung_up(slow).await });
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
}

/// Hop-by-hop headers, and the ones a client's `Connection` lists, stay on
/// the client's connection; the rest reach the node.
#[tokio::test(flavor = "multi_thread")]
async fn the_proxy_drops_hop_by_hop_headers() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo = listener.local_addr().unwrap();
    tokio::spawn(nanovm::server::serve_http1(listener, |req: http::Request<hyper::body::Incoming>| {
        let names: Vec<_> = req.headers().keys().map(|k| k.to_string()).collect();
        async move { nanovm::server::ok_json(serde_json::json!(names)) }
    }));
    let records = Arc::new(parking_lot::Mutex::default());
    let proxy = proxied(echo, doh(records.clone()).await).await;
    let counter = publish("counter", &app("counter")).await;
    let txt = format!("app={}:latest; trust=dev dev", counter.split_once('@').unwrap().0);
    records.lock().insert("_nanovm.hop.test".to_string(), vec![txt]);
    let head = "GET /a/ HTTP/1.1\r\nHost: hop.test\r\nConnection: close, x-private\r\nx-private: 1\r\n\
        Keep-Alive: timeout=5\r\nTE: trailers\r\nProxy-Authorization: Basic eA==\r\nx-kept: 1\r\n\r\n";
    let (status, body) = raw(proxy, head, b"").await;
    assert_eq!(status, 200, "{body}");
    let names: Vec<String> = serde_json::from_str(&body).unwrap();
    assert!(names.contains(&"x-kept".to_string()) && names.contains(&"nanovm-app".to_string()), "{names:?}");
    for gone in ["x-private", "keep-alive", "te", "proxy-authorization", "connection"] {
        assert!(!names.contains(&gone.to_string()), "{gone} reached the node: {names:?}");
    }
    // WebSocket upgrades terminate here, not at the node; malformed ones fail.
    let head = "GET /a/ HTTP/1.1\r\nHost: hop.test\r\nConnection: Upgrade, close, x-private\r\nUpgrade: websocket\r\n\r\n";
    assert_eq!(raw(proxy, head, b"").await.0, 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn front_door_events_cover_failures_and_link_to_worker() {
    use http_body_util::BodyExt;
    use opentelemetry_proto::tonic::{collector::trace::v1::ExportTraceServiceRequest, common::v1::any_value::Value};
    use prost::Message;
    let spans = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", sink.local_addr().unwrap());
    let captured = spans.clone();
    let collector = tokio::spawn(nanovm::server::serve_http1(sink, move |req| {
        let spans = captured.clone();
        async move {
            let trace = req.uri().path() == "/v1/traces";
            let body = req.into_body().collect().await.unwrap().to_bytes();
            if trace {
                assert!(!body.windows(7).any(|s| s == b"PRIVATE"));
                spans.lock().extend(ExportTraceServiceRequest::decode(body).unwrap().resource_spans
                    .into_iter().flat_map(|r| r.scope_spans).flat_map(|s| s.spans));
            }
            nanovm::server::ok_json(serde_json::json!({}))
        }
    }));
    let mut cfg = node_cfg("front_door_events", 8);
    cfg.telemetry_endpoint = endpoint.clone();
    let (sup, node) = supervised_with(cfg).await;
    let counter = publish("front-door", &app("counter")).await;
    let records = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    records.lock().insert("_nanovm.good.test".into(), vec![format!("app={}:latest; trust=dev dev", counter.split_once('@').unwrap().0)]);
    let cfg = nanovm::config::ProxyConfig {
        node: node.to_string(), resolver: doh(records).await, telemetry_endpoint: endpoint,
        listen: "127.0.0.1:0".into(), dev: true, ..Default::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    let service = tokio::spawn(nanovm::proxy::serve(nanovm::proxy::Proxy::new(cfg).unwrap(), listener));
    let missing = format!("{}@sha256:{}", counter.split_once('@').unwrap().0, "0".repeat(64));
    let cases = [
        (node, "x", "/obj/", String::new(), 400),
        (node, "x", "/obj/", "nanovm-app: invalid\r\n".into(), 400),
        (node, "x", "/../", names(&counter), 400),
        (node, "x", "/obj/", names(&missing), 502),
        (proxy, "absent.test", "/obj/", String::new(), 404),
        (proxy, "good.test", "/obj/", "Content-Type: application/websocket-events\r\n".into(), 400),
        (proxy, "good.test", "/obj/", String::new(), 200),
    ];
    for (i, (addr, host, path, headers, status)) in cases.iter().enumerate() {
        let head = format!("GET {path}?secret=PRIVATE HTTP/1.1\r\nHost: {host}\r\n{headers}traceparent: 00-{:032x}-1111111111111111-01\r\nConnection: close\r\n\r\n", i + 1);
        assert_eq!(raw(*addr, &head, b"").await.0, *status, "case {i}");
    }
    sup.flush_telemetry().await.unwrap();
    eventually("front-door exports", || spans.lock().len() >= 9).await;
    let spans = spans.lock();
    assert_eq!(spans.len(), 9, "six rejections, one complete three-hop request");
    for (i, (_, _, path, _, status)) in cases.iter().enumerate() {
        let mut trace = vec![0; 16];
        trace[15] = (i + 1) as u8;
        let group: Vec<_> = spans.iter().filter(|s| s.trace_id == trace).collect();
        let root = group.iter().find(|s| s.parent_span_id == [0x11; 8]).unwrap();
        assert_eq!(group.len(), if *status == 200 { 3 } else { 1 });
        assert_eq!(root.kind, 2);
        assert_eq!(root.status.as_ref().map_or(0, |s| s.code), if *status == 502 { 2 } else { 0 });
        for (key, value) in [("url.path", Value::StringValue((*path).into())),
            ("http.response.status_code", Value::IntValue(*status as i64))] {
            assert!(root.attributes.iter().any(|a| a.key == key && a.value.as_ref().and_then(|v| v.value.as_ref()) == Some(&value)));
        }
        if *status == 200 {
            let supervisor = group.iter().find(|s| s.name == "supervisor.request").unwrap();
            let worker = group.iter().find(|s| s.name == "request").unwrap();
            assert_eq!(root.name, "proxy.request");
            assert_eq!(supervisor.parent_span_id, root.span_id);
            assert_eq!(worker.parent_span_id, supervisor.span_id);
        }
    }
    service.abort();
    collector.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn post_return_work_keeps_context_and_reports_commit_failure() {
    use http_body_util::BodyExt;
    use opentelemetry_proto::tonic::{collector, common::v1::any_value::Value};
    use prost::Message;
    // A private fixture, outside guests/** and its publishing workflow.
    let build = std::process::Command::new("cargo").args([
        "build", "--locked", "--release", "--target", "wasm32-wasip2", "--manifest-path",
        "crates/nanovm/tests/post-return/Cargo.toml", "--target-dir", "target/test-guest",
    ]).current_dir(repo()).status().unwrap();
    assert!(build.success());
    let wasm = std::fs::read(repo().join("target/test-guest/wasm32-wasip2/release/post_return_probe.wasm")).unwrap();
    let spans = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let logs = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = node_cfg("post_return_context", 8);
    cfg.telemetry_endpoint = format!("http://{}", sink.local_addr().unwrap());
    let (captured, captured_logs) = (spans.clone(), logs.clone());
    let collector = tokio::spawn(nanovm::server::serve_http1(sink, move |req| {
        let (spans, logs) = (captured.clone(), captured_logs.clone());
        async move {
            let trace = req.uri().path() == "/v1/traces";
            let body = req.into_body().collect().await.unwrap().to_bytes();
            if trace {
                spans.lock().extend(collector::trace::v1::ExportTraceServiceRequest::decode(body).unwrap()
                    .resource_spans.into_iter().flat_map(|r| r.scope_spans).flat_map(|s| s.spans));
            } else {
                logs.lock().extend(collector::logs::v1::ExportLogsServiceRequest::decode(body).unwrap()
                    .resource_logs.into_iter().flat_map(|r| r.scope_logs).flat_map(|s| s.log_records));
            }
            nanovm::server::ok_json(serde_json::json!({}))
        }
    }));
    let gate = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = gate.local_addr().unwrap();
    let begin = Arc::new(tokio::sync::Semaphore::new(0));
    let finish = Arc::new(tokio::sync::Semaphore::new(0));
    let (arrived, mut arrivals) = tokio::sync::mpsc::channel(4);
    let (b, f) = (begin.clone(), finish.clone());
    let server = tokio::spawn(nanovm::server::serve_http1(gate, move |req| {
        let gate = if req.uri().path() == "/begin" { b.clone() } else { f.clone() };
        let arrived = arrived.clone();
        async move {
            arrived.send(req.uri().path().to_owned()).await.unwrap();
            gate.acquire().await.unwrap().forget();
            nanovm::server::ok_json(serde_json::json!({}))
        }
    }));
    let mut app = app("hostile");
    app.limits.egress = vec![address.to_string()];
    app.limits.post_return_timeout_ms = 5000;
    app.limits.cpu_ms = 1000;
    let objects = cfg.app_dirs(&app.name)[0].clone();
    let node = Node::new(cfg.clone(), &app, &wasm, None).unwrap();
    for (i, fail) in [(1u8, false), (2, true)] {
        let id = format!("parent{i}");
        let req = http::Request::builder().uri(format!("/?gate={address}"))
            .header("traceparent", format!("00-{i:032x}-1111111111111111-01"))
            .body(bytes::Bytes::new()).unwrap();
        let response = node.dispatch(&id, req).await;
        assert_eq!(response.status(), 200);
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), arrivals.recv()).await.unwrap().unwrap(), "/begin");
        begin.add_permits(1);
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), arrivals.recv()).await.unwrap().unwrap(), "/finish");
        let journal = objects.join("~journal").join(&id).join("log");
        assert!(journal.exists(), "post-return write has an uncommitted journal");
        if fail { std::fs::rename(&journal, journal.with_file_name("saved-log")).unwrap(); }
        let mut queued = if !fail {
            let node = node.clone();
            let id = id.clone();
            Some(tokio::spawn(async move {
                node.dispatch(&id, http::Request::builder().uri("/callee").body(bytes::Bytes::new()).unwrap()).await
            }))
        } else { None };
        if let Some(next) = &mut queued {
            assert!(tokio::time::timeout(Duration::from_millis(100), next).await.is_err(), "next request entered during post-return work");
        }
        finish.add_permits(1);
        assert_eq!(response.into_body().collect().await.unwrap().to_bytes(), "finished");
        if let Some(next) = queued {
            let next = tokio::time::timeout(Duration::from_secs(5), next).await.unwrap().unwrap();
            assert_eq!(next.into_body().collect().await.unwrap().to_bytes(), "callee");
        }
        eventually("final commit", || node.phases().keys().all(|p| *p == "idle" || *p == "cold")).await;
        if fail { std::fs::rename(journal.with_file_name("saved-log"), &journal).unwrap(); }
    }
    node.shutdown().await;
    let telemetry = node.telemetry.clone();
    drop(node);
    // The injected failure happened before log deletion: restoring that
    // undo record must retain the first commit and reject only later work.
    // This is not a claim that every ambiguous commit failure rolls back.
    for _ in 0..2 {
        let reopened = Node::new(cfg.clone(), &app, &wasm, None).unwrap();
        for id in ["parent1", "parent2"] {
            let response = reopened.dispatch(id, http::Request::builder().uri("/callee").body(bytes::Bytes::new()).unwrap()).await;
            assert_eq!(response.status(), 200);
            response.into_body().collect().await.unwrap();
            assert_eq!(std::fs::read(objects.join(id).join("before.txt")).unwrap(), b"before");
        }
        assert_eq!(std::fs::read(objects.join("parent1/after.txt")).unwrap(), b"after");
        assert!(!objects.join("parent2/after.txt").exists());
        reopened.shutdown().await;
    }
    tokio::task::spawn_blocking(move || telemetry.flush().unwrap()).await.unwrap();
    let spans = spans.lock();
    let logs = logs.lock();
    let attr = |attrs: &[opentelemetry_proto::tonic::common::v1::KeyValue], key: &str| {
        attrs.iter().find(|a| a.key == key).and_then(|a| a.value.as_ref()).and_then(|v| v.value.clone())
    };
    for (i, fail) in [(1u8, false), (2, true)] {
        let mut trace = vec![0; 16];
        trace[15] = i;
        let parent = spans.iter().find(|s| s.trace_id == trace && s.name == "request"
            && attr(&s.attributes, "nanovm.object") == Some(Value::StringValue(format!("parent{i}")))).unwrap();
        assert_eq!(attr(&parent.attributes, "http.response.status_code"), Some(Value::IntValue(200)));
        assert_eq!(parent.status.as_ref().map_or(0, |s| s.code), if fail { 2 } else { 0 });
        assert_eq!(attr(&parent.attributes, "error.type"), fail.then(|| Value::StringValue("post-return-commit-failed".into())));
        let call = spans.iter().find(|s| s.trace_id == trace && s.name == "call").unwrap();
        let child = spans.iter().find(|s| s.trace_id == trace && s.name == "request" && s.span_id != parent.span_id).unwrap();
        assert_eq!(call.parent_span_id, parent.span_id);
        assert_eq!(child.parent_span_id, call.span_id);
        let response_ms = match attr(&parent.attributes, "nanovm.response.duration_ms").unwrap() {
            Value::DoubleValue(ms) => ms, other => panic!("{other:?}"),
        };
        assert!(call.start_time_unix_nano > parent.start_time_unix_nano + (response_ms * 1e6) as u64);
        assert!(parent.end_time_unix_nano >= call.end_time_unix_nano);
        for (key, value) in [("log.iostream", "stderr")].into_iter()
            .chain(fail.then_some(("event.name", "post-return-commit-failed"))) {
            let log = logs.iter().find(|l| l.trace_id == trace && attr(&l.attributes, key) == Some(Value::StringValue(value.into()))).unwrap();
            assert_eq!(log.span_id, parent.span_id);
            assert!(matches!(log.body.as_ref().and_then(|v| v.value.as_ref()), Some(Value::StringValue(s)) if !s.is_empty() && s.len() <= 4096));
        }
    }
    server.abort();
    collector.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn jailed_workers_export_otel_without_guest_network_access() {
    use http_body_util::BodyExt;
    use opentelemetry_proto::tonic::{collector, common::v1::any_value::Value};
    use prost::Message;
    let batches = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let failures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let unavailable = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let received = batches.clone();
    let (down, rejected) = (unavailable.clone(), failures.clone());
    let collector = tokio::spawn(nanovm::server::serve_http1(listener, move |req| {
        let received = received.clone();
        let (down, rejected) = (down.clone(), rejected.clone());
        async move {
            let path = req.uri().path().to_string();
            let body = req.into_body().collect().await.unwrap().to_bytes();
            if down.load(Relaxed) {
                rejected.fetch_add(1, Relaxed);
                return nanovm::object::error_response(503, "collector-down", "");
            }
            received.lock().push((path, body));
            nanovm::server::ok_json(serde_json::json!({}))
        }
    }));
    let mut cfg = node_cfg("jailed_workers_export_otel_without_guest_network_access", 8);
    cfg.telemetry_endpoint = endpoint;
    let (_sup, addr) = supervised_with(cfg).await;
    let counter = publish("otel-counter", &app("counter")).await;
    let hostile = publish("otel-hostile", &app("hostile")).await;
    let head = format!("GET /room/ HTTP/1.1\r\nHost: x\r\n{}traceparent: 00-0123456789abcdef0123456789abcdef-0123456789abcdef-00\r\nConnection: close\r\n\r\n", names(&counter));
    assert_eq!(raw(addr, &head, b"").await.0, 200);
    let head = format!("GET /other/panic HTTP/1.1\r\nHost: x\r\n{}traceparent: 00-0123456789abcdef0123456789abcdef-0123456789abcdef-01\r\nConnection: close\r\n\r\n", names(&hostile));
    assert_eq!(raw(addr, &head, b"").await.0, 500);
    let (_, denied) = app_get(addr, &hostile, "/other/fetch?url=http://nanovm/telemetry/logs").await;
    assert!(denied.contains("HttpRequestDenied"), "{denied}");

    let attr = |attrs: &[opentelemetry_proto::tonic::common::v1::KeyValue], key: &str| {
        attrs.iter().find(|kv| kv.key == key).and_then(|kv| kv.value.as_ref()).and_then(|v| match &v.value {
            Some(Value::StringValue(s)) => Some(s.clone()), _ => None,
        }).unwrap_or_default()
    };
    let mut spans = Vec::new();
    let mut logs = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            for (path, body) in std::mem::take(&mut *batches.lock()) {
                match path.as_str() {
                    "/v1/traces" => spans.extend(collector::trace::v1::ExportTraceServiceRequest::decode(body).unwrap().resource_spans),
                    "/v1/logs" => logs.extend(collector::logs::v1::ExportLogsServiceRequest::decode(body).unwrap().resource_logs),
                    other => panic!("unexpected collector route {other}"),
                }
            }
            let complete = [&counter, &hostile].iter().all(|app| {
                let matches = |r: &Option<opentelemetry_proto::tonic::resource::v1::Resource>| {
                    r.as_ref().is_some_and(|r| attr(&r.attributes, "nanovm.app") == worker(app))
                };
                spans.iter().any(|r| matches(&r.resource) && r.scope_spans.iter().flat_map(|s| &s.spans).any(|s| s.name == "request"))
                    && logs.iter().any(|r| matches(&r.resource) && r.scope_logs.iter().flat_map(|s| &s.log_records)
                        .any(|l| attr(&l.attributes, "event.name") == "nanovm.snapshot"))
            });
            if complete && logs.iter().filter(|r| attr(&r.resource.as_ref().unwrap().attributes, "nanovm.app") == worker(&hostile))
                .flat_map(|r| &r.scope_logs).flat_map(|s| &s.log_records)
                .any(|l| attr(&l.attributes, "event.name") == "trap") { break }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("request and infrastructure events from real jailed workers");
    {
        for app in [&counter, &hostile] {
            let resource = spans.iter().find(|r| attr(&r.resource.as_ref().unwrap().attributes, "nanovm.app") == worker(app)).unwrap();
            assert_eq!(attr(&resource.resource.as_ref().unwrap().attributes, "service.version"), app.split_once('@').unwrap().1);
        }
        let counter_spans = spans.iter().filter(|r| attr(&r.resource.as_ref().unwrap().attributes, "nanovm.app") == worker(&counter))
            .flat_map(|r| &r.scope_spans).flat_map(|s| &s.spans).collect::<Vec<_>>();
        assert_eq!(counter_spans.len(), 1, "one rich event, not separate phase spans");
        let request = counter_spans.iter().find(|s| s.name == "request").unwrap();
        assert_eq!(request.trace_id, [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef].repeat(2));
        let hop = spans.iter().flat_map(|r| &r.scope_spans).flat_map(|s| &s.spans)
            .find(|s| s.name == "supervisor.request" && s.trace_id == request.trace_id
                && attr(&s.attributes, "nanovm.object") == "room").unwrap();
        assert_eq!(request.parent_span_id, hop.span_id);
        assert_eq!(hop.parent_span_id, [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
        assert_eq!(attr(&request.attributes, "nanovm.object"), "room");
        assert_eq!(attr(&request.attributes, "http.request.method"), "GET");
        assert_eq!(attr(&request.attributes, "url.path"), "/");
        assert!(request.attributes.iter().any(|a| a.key == "http.response.status_code"
            && a.value.as_ref().and_then(|v| v.value.as_ref()) == Some(&Value::IntValue(200))));
        for name in ["queue", "instantiate", "guest", "commit", "post_return", "post_return_commit"] {
            assert!(request.attributes.iter().any(|a| a.key == format!("nanovm.{name}.duration_ms")
                && matches!(a.value.as_ref().and_then(|v| v.value.as_ref()), Some(Value::DoubleValue(v)) if *v >= 0.0)), "{name}");
        }
    }
    let hostile_spans = spans.iter().filter(|r| attr(&r.resource.as_ref().unwrap().attributes, "nanovm.app") == worker(&hostile))
        .flat_map(|r| &r.scope_spans).flat_map(|s| &s.spans).collect::<Vec<_>>();
    let panic = hostile_spans.iter().find(|s| attr(&s.attributes, "url.path") == "/panic").unwrap();
    assert_eq!(panic.status.as_ref().unwrap().code, 2);
    assert_eq!(attr(&panic.attributes, "error.type"), "trap");
    let trap = logs.iter().flat_map(|r| &r.scope_logs).flat_map(|s| &s.log_records)
        .find(|l| attr(&l.attributes, "event.name") == "trap").unwrap();
    assert_eq!(trap.trace_id, panic.trace_id, "runtime diagnostics carry causal context too");
    assert_eq!(trap.span_id, panic.span_id);
    let restarted = hostile_spans.iter().find(|s| attr(&s.attributes, "url.path") == "/fetch").unwrap();
    assert_ne!(panic.trace_id, restarted.trace_id, "restart must use the next request's context");
    assert!(restarted.attributes.iter().any(|a| a.key == "nanovm.cold_start"
        && a.value.as_ref().and_then(|v| v.value.as_ref()) == Some(&Value::BoolValue(true))));
    assert!(logs.iter().filter(|r| attr(&r.resource.as_ref().unwrap().attributes, "nanovm.app") == worker(&hostile))
        .flat_map(|r| &r.scope_logs).flat_map(|s| &s.log_records).any(|l| {
            attr(&l.attributes, "nanovm.object") == "other" && attr(&l.attributes, "log.iostream") == "stderr"
                && l.trace_id == [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef].repeat(2)
                && l.span_id == panic.span_id
                && matches!(l.body.as_ref().and_then(|v| v.value.as_ref()), Some(Value::StringValue(s)) if s.contains("hostile guest panicked"))
        }));
    for (app, field, expected) in [(&counter, "nanovm.objects.phase.idle", 1),
        (&hostile, "nanovm.poisons.trap", 1), (&hostile, "nanovm.objects.limit", 8)] {
        assert!(logs.iter().filter(|r| attr(&r.resource.as_ref().unwrap().attributes, "nanovm.app") == worker(app))
            .flat_map(|r| &r.scope_logs).flat_map(|s| &s.log_records).any(|l|
                attr(&l.attributes, "event.name") == "nanovm.snapshot" && l.attributes.iter().any(|a|
                    a.key == field && a.value.as_ref().and_then(|v| v.value.as_ref()) == Some(&Value::IntValue(expected)))), "{field}");
    }
    // Concurrent requests queue on one caller. Each object's outbound call
    // must inherit that request, not another queued request's context.
    futures::future::join_all((1..=6).map(|i| {
        let app = &hostile;
        async move {
            let target = match i {
                4 => "callee4/panic".to_owned(),
                5 => "caller/".to_owned(),
                6 => "callee6/missing".to_owned(),
                _ => format!("callee{i}/sleep?ms=5"),
            };
            let head = format!("GET /caller/fetch?url=http://nanovm/objects/{target} HTTP/1.1\r\nHost: x\r\n{}traceparent: 00-{i:032x}-1111111111111111-01\r\nConnection: close\r\n\r\n", names(app));
            let (status, body) = raw(addr, &head, b"").await;
            assert_eq!(status, 200, "{body}");
            let expected = match i { 4 => 500, 5 => 508, 6 => 404, _ => 200 };
            assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["status"], expected);
        }
    })).await;
    let mut calls = Vec::new();
    let mut hops = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            _sup.flush_telemetry().await.unwrap();
            for (path, body) in std::mem::take(&mut *batches.lock()) {
                if path != "/v1/traces" { continue }
                for resource in collector::trace::v1::ExportTraceServiceRequest::decode(body).unwrap().resource_spans {
                    if attr(&resource.resource.as_ref().unwrap().attributes, "nanovm.app") == "_supervisor" {
                        hops.extend(resource.scope_spans.into_iter().flat_map(|s| s.spans));
                        continue;
                    }
                    assert_eq!(attr(&resource.resource.as_ref().unwrap().attributes, "nanovm.app"), worker(&hostile));
                    calls.extend(resource.scope_spans.into_iter().flat_map(|s| s.spans));
                }
            }
            if calls.len() >= 17 && hops.len() >= 6 { break }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("all queued calls exported");
    assert_eq!(calls.len(), 17);
    for i in 1..=6 {
        let mut trace = vec![0; 16];
        trace[15] = i;
        let group: Vec<_> = calls.iter().filter(|s| s.trace_id == trace).collect();
        assert_eq!(group.len(), if i == 5 { 2 } else { 3 }, "a rejected cycle has no callee request");
        let caller = group.iter().find(|s| s.name == "request" && attr(&s.attributes, "nanovm.object") == "caller").unwrap();
        let call = group.iter().find(|s| s.name == "call").unwrap();
        let hop = hops.iter().find(|s| s.trace_id == trace).unwrap();
        assert_eq!(caller.parent_span_id, hop.span_id);
        assert_eq!(hop.parent_span_id, [0x11; 8]);
        assert_eq!(call.parent_span_id, caller.span_id);
        assert_eq!(caller.kind, 2, "SERVER request");
        assert_eq!(call.kind, 3, "CLIENT call");
        let expected = match i { 4 => 500, 5 => 508, 6 => 404, _ => 200 };
        assert!(call.attributes.iter().any(|a| a.key == "http.response.status_code"
            && a.value.as_ref().and_then(|v| v.value.as_ref()) == Some(&Value::IntValue(expected))));
        assert_eq!(attr(&call.attributes, "nanovm.target_object"), if i == 5 { "caller".to_owned() } else { format!("callee{i}") });
        assert_eq!(attr(&call.attributes, "http.request.method"), "GET");
        let path = match i { 4 => "/objects/callee4/panic".to_owned(), 5 => "/objects/caller/".to_owned(), 6 => "/objects/callee6/missing".to_owned(), _ => format!("/objects/callee{i}/sleep") };
        assert_eq!(attr(&call.attributes, "url.path"), path, "query values are not exported");
        assert_eq!(call.status.as_ref().map_or(0, |s| s.code), if i >= 4 { 2 } else { 0 });
        assert_eq!(attr(&call.attributes, "error.type"), match i { 4 => "trap", 5 => "call-cycle", 6 => "404", _ => "" });
        assert_eq!(caller.status.as_ref().map_or(0, |s| s.code), 0, "the caller handled the dependency failure");
        if i != 5 {
            let callee = group.iter().find(|s| s.name == "request" && attr(&s.attributes, "nanovm.object") == format!("callee{i}")).unwrap();
            assert_eq!(callee.parent_span_id, call.span_id);
            assert_eq!(callee.kind, 2);
            assert_eq!(callee.status.as_ref().map_or(0, |s| s.code), if i == 4 { 2 } else { 0 }, "404 is not a server failure");
        }
    }
    // Refuse exports on existing connections too, and prove an export failed
    // before checking that requests still succeed.
    unavailable.store(true, Relaxed);
    tokio::time::timeout(Duration::from_secs(5), async {
        while failures.load(Relaxed) == 0 { tokio::time::sleep(Duration::from_millis(20)).await; }
    }).await.expect("collector must actually refuse an export");
    assert_eq!(tokio::time::timeout(Duration::from_secs(2), app_get(addr, &counter, "/room/")).await.unwrap().0, 200);
    collector.abort();
}
