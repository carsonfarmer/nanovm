//! Hostile guest, second edition (D-58): attacks the first edition doesn't
//! make, each shown contained. The component is `crates/nanovm/tests/hostile2`
//! (outside guests/**). Notes and numbers: `notes/quality.md`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use nanovm::config::{AppConfig, NodeConfig};
use nanovm::table::Node;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;

use load::{Reply, request};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn guest() -> Vec<u8> {
    static BUILT: std::sync::Once = std::sync::Once::new();
    BUILT.call_once(|| {
        let build = std::process::Command::new("cargo").args([
            "build", "--locked", "--release", "--target", "wasm32-wasip2", "--manifest-path",
            "crates/nanovm/tests/hostile2/Cargo.toml", "--target-dir", "target/test-guest",
        ]).current_dir(repo()).status().unwrap();
        assert!(build.success());
    });
    std::fs::read(repo().join("target/test-guest/wasm32-wasip2/release/hostile2.wasm")).unwrap()
}

/// The first edition's limits (`hostile`), egress to loopback (dev mode).
fn app() -> AppConfig {
    let limits = serde_json::json!({
        "memory_mb": 32, "cpu_ms": 100, "request_timeout_ms": 1000, "post_return_timeout_ms": 500,
        "disk_mb": 8, "max_files": 200, "max_outbound_concurrency": 2, "egress": ["127.0.0.1"],
    });
    AppConfig {
        name: "hostile2".into(), digest: "test".into(), env: Default::default(), config: Default::default(),
        limits: serde_json::from_value(limits).unwrap(), worker: Default::default(),
    }
}

fn node_with(test: &str, app: AppConfig) -> Arc<Node> {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let dir = root.join("nodes").join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = NodeConfig { data_dir: dir, max_live_objects: 16, memory_reservation_mb: 64, dev: true, ..NodeConfig::default() };
    cfg.validate().unwrap();
    nanovm::engine::preflight(&cfg).unwrap();
    Node::new(cfg, &app, &guest(), None).unwrap()
}

fn node(test: &str) -> Arc<Node> {
    node_with(test, app())
}

async fn get(node: &Arc<Node>, id: &str, path: &str) -> Reply {
    request(node, "GET", id, path, "").await
}

/// Runs `attack` on object `bad` while a canary object is asked for `/`
/// in a loop; the attack's reply and the canary's slowest answer.
async fn beside_canary(node: &Arc<Node>, attack: &str) -> (Reply, Duration) {
    assert_eq!(get(node, "canary", "/").await.status, 200);
    let attacker = { let (node, attack) = (node.clone(), attack.to_string()); tokio::spawn(async move { get(&node, "bad", &attack).await }) };
    let mut worst = Duration::ZERO;
    while !attacker.is_finished() {
        let r = get(node, "canary", "/").await;
        assert_eq!(r.status, 200, "{}", r.body);
        worst = worst.max(r.elapsed);
    }
    (attacker.await.unwrap(), worst)
}

/// Unbounded recursion traps (wasm stack exhaustion) instead of taking the
/// host down; neighbours carry on and the object restarts.
#[tokio::test(flavor = "multi_thread")]
async fn runaway_recursion_traps() {
    let node = node("hostile2_recursion");
    let (r, worst) = beside_canary(&node, "/recurse").await;
    assert_eq!((r.status, r.error.as_deref()), (500, Some("trap")), "{}", r.body);
    // Rust's shadow stack lives in linear memory, so it may run out there first.
    assert!(["call stack exhausted", "out of bounds memory access"].iter().any(|t| r.body.contains(t)), "{}", r.body);
    assert!(worst < Duration::from_millis(200), "canary waited {worst:?}");
    assert_eq!(get(&node, "bad", "/").await.status, 200);
}

/// Exponential backtracking (`(a+)+$` on `a…a!`, as a naive regex engine
/// runs it) is cut off by the CPU budget, well before the wall clock.
#[tokio::test(flavor = "multi_thread")]
async fn pathological_backtracking_is_stopped_by_the_cpu_budget() {
    let node = node("hostile2_regex");
    let (r, worst) = beside_canary(&node, "/regex?n=40").await;
    assert_eq!((r.status, r.error.as_deref()), (500, Some("cpu")), "{}", r.body);
    assert!(r.elapsed < Duration::from_millis(800), "took {:?}", r.elapsed);
    assert!(worst < Duration::from_millis(200), "canary waited {worst:?}");
    assert_eq!(get(&node, "bad", "/regex?n=8").await.body, "false");
}

/// Header names chosen to share one bucket of `http::HeaderMap`'s fast
/// hash cost about what ordinary names do: the map switches to a keyed
/// hash after a long probe, and the 64 KiB limit on a map caps the count
/// (each entry also counts some 64 bytes of overhead: about 830 short names fit).
#[tokio::test(flavor = "multi_thread")]
async fn colliding_header_names_cost_no_more_than_ordinary_ones() {
    let mut app = app();
    app.limits.cpu_ms = 2000;
    app.limits.request_timeout_ms = 5000;
    let node = node_with("hostile2_hashflood", app);
    let (mut plain, mut colliding) = (vec![], vec![]);
    for _ in 0..5 {
        for (collide, out) in [(0, &mut plain), (1, &mut colliding)] {
            let r = get(&node, "h", &format!("/hashflood?n=800&collide={collide}&reps=21")).await;
            assert_eq!(r.status, 200, "{}", r.body);
            out.push(r.body.parse::<u64>().unwrap());
        }
    }
    plain.sort();
    colliding.sort();
    let (p, c) = (plain[2], colliding[2]);
    eprintln!("800 names per map, median ns per build: ordinary {plain:?}, colliding {colliding:?}");
    assert!(c <= 3 * p + 1_000_000, "colliding names cost {c} ns, ordinary {p} ns");
}

/// A guest's response can hold only as many headers as 64 KiB has room for,
/// and a client's request with thousands of headers is refused by the
/// node's HTTP server, which carries on serving.
#[tokio::test(flavor = "multi_thread")]
async fn huge_header_counts_are_bounded_both_ways() {
    let node = node("hostile2_headers");
    let res = node.dispatch("h", http::Request::get("http://localhost/headers?n=1000000").body(bytes::Bytes::new()).unwrap()).await;
    let count = res.headers().len();
    let size: usize = res.headers().iter().map(|(k, v)| k.as_str().len() + v.len()).sum();
    eprintln!("a guest asking for 1,000,000 response headers got {count} ({size} bytes of names and values)");
    assert!(size <= 64 << 10 && count < 10_000, "{count} headers, {size} bytes");

    let addr = serve(node.clone()).await;
    let mut head = String::from("GET /h/ HTTP/1.1\r\nHost: x\r\n");
    for i in 0..5000 {
        head += &format!("x-{i}: v\r\n");
    }
    head += "Connection: close\r\n\r\n";
    let (status, _) = raw(addr, head.as_bytes()).await;
    assert_eq!(status, 431, "5,000 request headers");
    let (status, body) = raw(addr, b"GET /h/ HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
    assert_eq!((status, body.as_str()), (200, "ok"));
}

/// A node's HTTP front end on loopback.
async fn serve(node: Arc<Node>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(nanovm::server::serve_http1(listener, move |req| nanovm::server::handle(node.clone(), req)));
    addr
}

/// Sends `bytes` and reads the reply to the end: its status and body.
async fn raw(addr: std::net::SocketAddr, bytes: &[u8]) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(bytes).await.unwrap();
    let mut out = vec![];
    let _ = s.read_to_end(&mut out).await;
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0);
    (status, text.split_once("\r\n\r\n").map_or(String::new(), |(_, b)| b.to_string()))
}

/// A slow upload holds its connection, not its object: the body is read
/// before the request is queued, so the object answers others meanwhile.
/// Nothing bounds how long the upload may take, though (N-503).
#[tokio::test(flavor = "multi_thread")]
async fn slow_uploads_hold_a_connection_not_the_object() {
    let node = node("hostile2_slow_upload");
    let addr = serve(node.clone()).await;
    let mut slow = tokio::net::TcpStream::connect(addr).await.unwrap();
    slow.write_all(b"POST /u/upload HTTP/1.1\r\nHost: x\r\nContent-Length: 40\r\nConnection: close\r\n\r\n").await.unwrap();
    let t0 = Instant::now();
    for i in 0..40 {
        slow.write_all(b"x").await.unwrap();
        if i % 10 == 0 {
            let (status, body) = raw(addr, b"GET /u/ HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
            assert_eq!((status, body.as_str()), (200, "ok"), "the uploading object answers others");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Four seconds for 40 bytes, past every per-request limit the app has.
    assert!(t0.elapsed() > Duration::from_millis(app().limits.request_timeout_ms * 3));
    let mut out = vec![];
    slow.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out);
    assert!(text.starts_with("HTTP/1.1 200") && text.ends_with("40"), "{text}");
}

/// An upstream on loopback that holds each request for 300 ms and counts
/// the most it held at once.
async fn upstream() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (now, max) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let peak = max.clone();
    tokio::spawn(nanovm::server::serve_http1(listener, move |_req| {
        let (now, max) = (now.clone(), max.clone());
        async move {
            let n = now.fetch_add(1, Relaxed) + 1;
            max.fetch_max(n, Relaxed);
            tokio::time::sleep(Duration::from_millis(300)).await;
            now.fetch_sub(1, Relaxed);
            nanovm::object::error_response(200, "ok", "")
        }
    }));
    (addr, peak)
}

/// Fifty outbound requests at once reach the upstream at most
/// `max_outbound_concurrency` at a time; the rest are refused at once.
#[tokio::test(flavor = "multi_thread")]
async fn outbound_connection_storms_are_capped() {
    let node = node("hostile2_storm");
    let (up, peak) = upstream().await;
    let r = get(&node, "s", &format!("/storm?n=50&url=http://{up}")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let counts: Vec<usize> = r.body.split(' ').map(|n| n.parse().unwrap()).collect();
    eprintln!("storm of 50: answered {}, refused (connection limit) {}, other errors {}; upstream peak {}",
        counts[0], counts[1], counts[2], peak.load(Relaxed));
    assert!(peak.load(Relaxed) <= 2, "upstream saw {} at once", peak.load(Relaxed));
    assert_eq!(counts.iter().sum::<usize>(), 50);
    assert!(counts[0] >= 1 && counts[0] <= 2 && counts[1] >= 48, "{counts:?}");
}

/// A request that renames one file back and forth as fast as it can, then
/// fails: the request ends at its deadline or the journal's budget, rolls
/// back, the file is as it was, and the next start's recovery is bounded.
#[tokio::test(flavor = "multi_thread")]
async fn rename_storms_end_and_roll_back() {
    let node = node("hostile2_renames");
    assert_eq!(get(&node, "r", "/renames?n=0").await.status, 200);
    let t0 = Instant::now();
    let (r, worst) = beside_canary(&node, "/renames?n=10000000&then=fail").await;
    let storm = t0.elapsed();
    assert!(r.status >= 500, "{} {}", r.status, r.body);
    let t1 = Instant::now();
    let listing = get(&node, "r", "/ls").await;
    let recovery = t1.elapsed();
    eprintln!("rename storm: {} after {storm:?} ({}); next request (recovery) {recovery:?}; canary worst {worst:?}",
        r.status, r.body.chars().take(120).collect::<String>());
    assert_eq!(listing.body, "f\t7", "{}", listing.body);
    assert!(recovery < storm + Duration::from_secs(2), "recovery {recovery:?} after a {storm:?} storm");
    assert!(worst < Duration::from_millis(500), "canary waited {worst:?}");
}

/// Names at and past the length limit and unusual Unicode ones commit and
/// roll back exactly, and survive a restart's rescan.
#[tokio::test(flavor = "multi_thread")]
async fn unusual_names_commit_and_roll_back() {
    let node = node("hostile2_names");
    let r = get(&node, "n", "/names?then=fail").await;
    assert_eq!(r.status, 500, "{}", r.body);
    assert_eq!(get(&node, "n", "/ls").await.body, "", "a failed request left names behind");
    let r = get(&node, "n", "/names").await;
    assert_eq!(r.status, 200, "{}", r.body);
    let results: Vec<&str> = r.body.split(',').collect();
    assert_eq!(results.len(), 15, "{}", r.body);
    assert!(results.iter().enumerate().all(|(i, r)| if i == 13 { *r == "name-too-long" } else { *r == "ok" }), "{}", r.body);
    let listing = get(&node, "n", "/ls").await.body;
    assert_eq!(listing.split('\0').count(), 14, "{listing}");
    for line in listing.split('\0') {
        let (name, size) = line.split_once('\t').unwrap();
        assert_eq!(size, name.len().to_string(), "{name:?}");
    }
    // What a restart would rescan.
    let volume = node.cfg.app_dirs(&node.app.name)[0].join("n");
    let on_disk = std::fs::read_dir(&volume).unwrap().count();
    assert_eq!(on_disk, 14);
}

/// A chain of directories reached one descriptor at a time, as deep as the
/// guest likes. Where the volume's path to an entry would reach PATH_MAX the
/// create is refused (`name-too-long`) and the object carries on; a chain
/// that fits commits and rolls back like any other path. Before the fix,
/// the create at that depth failed its commit and then every recovery, and
/// the object answered 503 for good (N-504).
#[tokio::test(flavor = "multi_thread")]
async fn changes_deeper_than_path_max_are_refused_and_the_rest_roll_back() {
    let mut app = app();
    app.limits.cpu_ms = 2000;
    let node = node_with("hostile2_deep", app);
    let r = get(&node, "d", "/deep?depth=20&len=255&op=create").await;
    assert!(r.status == 500 && r.body.contains("name-too-long"), "{}", r.body);
    assert_eq!(get(&node, "d", "/").await.status, 200, "the object still starts");
    let r = get(&node, "d", "/deep?depth=12&len=255&op=create").await;
    assert_eq!((r.status, r.body.as_str()), (200, "ok"), "{}", r.body);
    assert_eq!(get(&node, "d", "/deep?depth=12&len=255&op=read").await.body, "committed");
    for op in ["overwrite", "unlink"] {
        let r = get(&node, "d", &format!("/deep?depth=12&len=255&op={op}&then=fail")).await;
        assert_eq!(r.status, 500, "{op}: {}", r.body);
        let r = get(&node, "d", "/deep?depth=12&len=255&op=read").await;
        assert_eq!((r.status, r.body.as_str()), (200, "committed"), "a failed {op} 12 directories down was not rolled back");
    }
}
