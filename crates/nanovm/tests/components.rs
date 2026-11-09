//! End-to-end tests of the Wasm components in `components/` (queue, ...),
//! each composed into a small test app and run on a real in-process node.
//! Build the fixtures first with `bash scripts/build-components.sh`; the tests
//! are ignored by default for that reason and run in CI with `--ignored`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use nanovm::config::{AppConfig, NodeConfig};
use nanovm::table::Node;
use serde_json::{Value, json};

#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;
use load::request;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// The composed test app of `component`.
fn fixture(component: &str) -> Vec<u8> {
    let p = repo().join("target/components").join(component).join("tests.wasm");
    std::fs::read(&p).unwrap_or_else(|_| panic!("{} is missing: run components/{component}/build.sh", p.display()))
}

/// Limits generous enough for a loaded CI host (the CPU budget counts wall
/// time inside guest polls), with the given overrides.
fn app(component: &str, overrides: Value) -> AppConfig {
    let mut limits = json!({
        "memory_mb": 64, "cpu_ms": 5000, "request_timeout_ms": 30000,
        "disk_mb": 16, "max_files": 200,
    });
    for (k, v) in overrides.as_object().unwrap() {
        limits[k] = v.clone();
    }
    AppConfig {
        name: component.into(),
        digest: "test".into(),
        env: Default::default(),
        config: Default::default(),
        limits: serde_json::from_value(limits).unwrap(),
        worker: Default::default(),
    }
}

fn node_cfg(test: &str, max_live: usize) -> NodeConfig {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let dir = root.join("components").join(test);
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

fn start(cfg: &NodeConfig, app: &AppConfig) -> Arc<Node> {
    nanovm::engine::preflight(cfg).unwrap();
    Node::new(cfg.clone(), app, &fixture(&app.name), None).unwrap()
}

/// A node restart: everything in memory goes, the volumes stay.
async fn restart(node: Arc<Node>, cfg: &NodeConfig, app: &AppConfig) -> Arc<Node> {
    node.shutdown().await;
    drop(node);
    start(cfg, app)
}

/// Evicts every live object of a one-slot node by visiting another object.
async fn evict(node: &Arc<Node>, path: &str) {
    let before = node.metrics.evicted_lru.load(Relaxed);
    settle(node).await;
    let r = request(node, "GET", "evictor", path, "").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(node.metrics.evicted_lru.load(Relaxed) > before, "nothing was evicted");
    settle(node).await;
}

async fn settle(node: &Arc<Node>) {
    let t0 = Instant::now();
    while !node.phases().keys().all(|p| *p == "idle") {
        assert!(t0.elapsed() < Duration::from_secs(10), "objects never went idle");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// `method path` on `id`, which must answer 200; its JSON.
async fn ok(node: &Arc<Node>, method: &str, id: &str, path: &str, body: &str) -> Value {
    let r = request(node, method, id, path, body).await;
    assert_eq!(r.status, 200, "{method} {path}: {} {:?}", r.body, r.error);
    r.json()
}

fn payloads(v: &Value) -> Vec<String> {
    v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["payload"].as_str().unwrap().to_string())
        .collect()
}

fn volume(cfg: &NodeConfig, app: &str, id: &str) -> PathBuf {
    cfg.data_dir.join("objects").join(app).join(id)
}

// ---- queue ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/queue/build.sh"]
async fn queue_contracts() {
    let cfg = node_cfg("queue_contracts", 4);
    let node = start(&cfg, &app("queue", json!({})));
    assert_eq!(ok(&node, "GET", "q", "/contracts", "").await, json!("ok"));
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/queue/build.sh"]
async fn queue_consumers_persist_across_eviction_and_restart() {
    let cfg = node_cfg("queue_persist", 1);
    let app = app("queue", json!({}));
    let mut node = start(&cfg, &app);
    assert_eq!(ok(&node, "POST", "q", "/append", "a\nb\nc").await["first"], 0);
    assert_eq!(ok(&node, "POST", "q", "/append", "d").await["first"], 3);
    let got = ok(&node, "POST", "q", "/next?max=2", "").await;
    assert_eq!(payloads(&got), ["a", "b"]);
    assert_eq!(got["messages"][1]["offset"], 1);
    // Two consumers move independently.
    assert_eq!(
        payloads(&ok(&node, "POST", "q", "/next?consumer=audit&max=10", "").await),
        ["a", "b", "c", "d"]
    );
    evict(&node, "/bounds").await;
    assert_eq!(payloads(&ok(&node, "POST", "q", "/next?max=10", "").await), ["c", "d"]);
    assert_eq!(ok(&node, "POST", "q", "/append", "e").await["first"], 4);
    node = restart(node, &cfg, &app).await;
    assert_eq!(
        ok(&node, "GET", "q", "/bounds", "").await,
        json!({"oldest": 0, "next": 5})
    );
    assert_eq!(payloads(&ok(&node, "POST", "q", "/next", "").await), ["e"]);
    assert_eq!(
        ok(&node, "GET", "q", "/consumers", "").await["consumers"],
        json!([["audit", 4], ["worker", 5]])
    );
    // Seeking back replays.
    ok(&node, "POST", "q", "/seek?offset=3", "").await;
    assert_eq!(payloads(&ok(&node, "POST", "q", "/next", "").await), ["d", "e"]);
    assert_eq!(
        payloads(&ok(&node, "GET", "q", "/read?offset=1&max=2", "").await),
        ["b", "c"]
    );
    // Objects don't share topics.
    settle(&node).await;
    assert_eq!(
        ok(&node, "GET", "other", "/bounds", "").await,
        json!({"oldest": 0, "next": 0})
    );
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/queue/build.sh"]
async fn queue_failed_request_rolls_back_appends_and_positions() {
    let cfg = node_cfg("queue_rollback", 2);
    let node = start(&cfg, &app("queue", json!({})));
    ok(&node, "POST", "q", "/append", "one\ntwo").await;
    // Processing failed: the position is not advanced, so they come again.
    let r = request(&node, "POST", "q", "/next?fail=1", "").await;
    assert_eq!(r.error.as_deref(), Some("guest-error"), "{}", r.body);
    assert_eq!(ok(&node, "GET", "q", "/position", "").await["position"], 0);
    let r = request(&node, "GET", "q", "/trap", "").await;
    assert_eq!(r.error.as_deref(), Some("trap"), "{}", r.body);
    assert_eq!(
        ok(&node, "GET", "q", "/bounds", "").await,
        json!({"oldest": 0, "next": 2})
    );
    assert_eq!(payloads(&ok(&node, "POST", "q", "/next", "").await), ["one", "two"]);
    assert_eq!(ok(&node, "POST", "q", "/append", "three").await["first"], 2);
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/queue/build.sh"]
async fn queue_segments_roll_and_trim() {
    let cfg = node_cfg("queue_trim", 2);
    let node = start(&cfg, &app("queue", json!({})));
    // 40,000 eight-byte messages: three segments of at most 16,384.
    ok(&node, "POST", "q", "/bulk?count=40000&size=8&batch=1024", "").await;
    let dir = volume(&cfg, "queue", "q").join(".queue/jobs");
    let segments = |dir: &Path| {
        let mut s: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| {
                e.unwrap()
                    .file_name()
                    .into_string()
                    .unwrap()
                    .strip_suffix(".idx")
                    .map(String::from)
            })
            .collect();
        s.sort();
        s
    };
    assert_eq!(
        segments(&dir),
        ["00000000000000000000", "00000000000000016384", "00000000000000032768"]
    );
    // A read across a segment boundary.
    let got = ok(&node, "GET", "q", "/read?offset=16382&max=4", "").await;
    assert_eq!(payloads(&got), ["00016382", "00016383", "00016384", "00016385"]);
    ok(&node, "POST", "q", "/seek?offset=100", "").await;
    ok(&node, "POST", "q", "/trim?before=20000", "").await;
    assert_eq!(segments(&dir), ["00000000000000016384", "00000000000000032768"]);
    assert_eq!(
        ok(&node, "GET", "q", "/bounds", "").await,
        json!({"oldest": 20000, "next": 40000})
    );
    // A consumer behind the trim point moves up to the oldest message.
    assert_eq!(ok(&node, "GET", "q", "/position", "").await["position"], 20000);
    assert_eq!(
        payloads(&ok(&node, "GET", "q", "/read?offset=5&max=1", "").await),
        ["00020000"]
    );
    let r = request(&node, "POST", "q", "/trim?before=40001", "").await;
    assert_eq!(r.status, 409, "{}", r.body);
    // Trimming everything keeps offsets increasing.
    ok(&node, "POST", "q", "/trim?before=40000", "").await;
    assert!(segments(&dir).is_empty());
    assert_eq!(ok(&node, "POST", "q", "/append", "after").await["first"], 40000);
    assert_eq!(
        ok(&node, "GET", "q", "/bounds", "").await,
        json!({"oldest": 40000, "next": 40001})
    );
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/queue/build.sh"]
async fn queue_quota_exhaustion_is_refused_cleanly() {
    let cfg = node_cfg("queue_quota", 2);
    let app = app("queue", json!({"disk_mb": 1}));
    let mut node = start(&cfg, &app);
    // 64 KiB messages until the 1 MiB quota refuses one; the probe checks
    // the refused append changed nothing, and the request commits the rest.
    let full = ok(&node, "POST", "q", "/fill", "").await;
    assert_eq!(full["error"], "Error::Full");
    let appended = full["appended"].as_u64().unwrap();
    assert!((10..16).contains(&appended), "{full}");
    node = restart(node, &cfg, &app).await;
    assert_eq!(ok(&node, "GET", "q", "/bounds", "").await["next"], appended);
    // Trimming frees the space at commit; appends work again.
    ok(&node, "POST", "q", &format!("/trim?before={appended}"), "").await;
    assert_eq!(ok(&node, "POST", "q", "/append", "fits").await["first"], appended);
    node.shutdown().await;
    // Out of directory entries: a new topic is refused whole, and an existing
    // consumer moves on while a new one is refused.
    let cfg = node_cfg("queue_files", 2);
    let app = crate::app("queue", json!({"max_files": 4}));
    let node = start(&cfg, &app);
    ok(&node, "POST", "q", "/append", "x").await; // .queue, jobs, .idx, .dat
    let r = request(&node, "POST", "q", "/append?topic=other", "y").await;
    assert_eq!(
        (r.status, r.json()["error"].as_str()),
        (409, Some("Error::Full")),
        "{}",
        r.body
    );
    let r = request(&node, "POST", "q", "/seek?offset=1", "").await;
    assert_eq!(
        (r.status, r.json()["error"].as_str()),
        (409, Some("Error::Full")),
        "{}",
        r.body
    );
    assert_eq!(ok(&node, "GET", "q", "/consumers", "").await["consumers"], json!([]));
    assert_eq!(
        ok(&node, "GET", "q", "/bounds?topic=other", "").await,
        json!({"oldest": 0, "next": 0})
    );
    let entries: Vec<_> = std::fs::read_dir(volume(&cfg, "queue", "q").join(".queue"))
        .unwrap()
        .collect();
    assert_eq!(entries.len(), 1, "a refused topic left a directory");
    node.shutdown().await;
}
