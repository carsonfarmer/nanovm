//! `tools/volume.py` against real nodes: an object's files, key-value and
//! SQLite data leave one data root and come back in a fresh one, offline (the
//! tool holds the app's storage lock) or from local capture while the node
//! runs. Build the guests first with `scripts/build-guests.sh`; the SQLite
//! test (ignored) also needs `experiments/sqlite-filesystem/build.sh --compose`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nanovm::config::{AppConfig, CaptureConfig, NodeConfig};
use nanovm::table::Node;

#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// The counter app with room for a composed SQLite client, as the backup
/// drill gives it, and CPU to spare on a loaded host.
fn app() -> AppConfig {
    let limits = serde_json::json!({
        "memory_mb": 64, "cpu_ms": 2000, "request_timeout_ms": 30000,
        "idle_timeout_ms": 60000, "disk_mb": 1, "max_files": 100,
    });
    AppConfig {
        name: "counter".into(),
        digest: "test".into(),
        env: Default::default(),
        config: Default::default(),
        limits: serde_json::from_value(limits).unwrap(),
        worker: Default::default(),
    }
}

fn node_cfg(test: &str) -> NodeConfig {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let dir = root.join("volume").join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cache = root.join("cwasm-cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::os::unix::fs::symlink(&cache, dir.join("cache")).unwrap();
    let mut cfg = NodeConfig {
        data_dir: dir.join("source"),
        max_live_objects: 8,
        memory_reservation_mb: 64,
        dev: true,
        ..NodeConfig::default()
    };
    cfg.limits.cpu_ms = 2000;
    cfg.limits.request_timeout_ms = 30000;
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    std::os::unix::fs::symlink(&cache, cfg.data_dir.join("cache")).unwrap();
    cfg
}

fn start(cfg: &NodeConfig, wasm: &[u8]) -> Arc<Node> {
    nanovm::engine::preflight(cfg).unwrap();
    Node::new(cfg.clone(), &app(), wasm, None).unwrap()
}

/// Shuts `node` down and waits until its app's storage lock is free.
async fn stop(node: Arc<Node>, cfg: &NodeConfig) {
    node.shutdown().await;
    drop(node);
    let dir = cfg.app_dirs("counter")[0].clone();
    let t0 = Instant::now();
    while std::fs::File::open(&dir).unwrap().try_lock().is_err() {
        assert!(t0.elapsed() < Duration::from_secs(5), "the app's storage stayed locked");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn volume(args: &[&dyn AsRef<std::ffi::OsStr>]) -> std::process::Output {
    let mut cmd = std::process::Command::new("python3");
    cmd.arg(repo().join("tools/volume.py"));
    for a in args {
        cmd.arg(a);
    }
    cmd.output().unwrap()
}

fn ok(out: std::process::Output) -> String {
    assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

async fn call(node: &Arc<Node>, method: &str, id: &str, path: &str, body: &str) -> String {
    let r = load::request(node, method, id, path, body).await;
    assert!(r.status < 300, "{method} {path}: {} {:?} {}", r.status, r.error, r.body);
    r.body
}

/// Files and key-value data move offline, holding the app's lock: the
/// export is refused while the node holds it; the fresh root's node reads
/// what the old one committed and keeps writing.
#[tokio::test(flavor = "multi_thread")]
async fn volume_export_import_moves_files_and_kv() {
    let cfg = node_cfg("files_and_kv");
    let wasm = std::fs::read(repo().join("guests/target/wasm32-wasip2/release/counter.wasm")).unwrap();
    let node = start(&cfg, &wasm);
    for _ in 0..3 {
        call(&node, "POST", "alice", "/incr", "").await;
    }
    call(&node, "PUT", "alice", "/kv/greeting", "hello from the old root").await;
    let out = cfg.data_dir.parent().unwrap().join("alice.tar");
    let refused = volume(&[&"export", &cfg.data_dir, &"counter", &"alice", &out]);
    assert!(!refused.status.success() && String::from_utf8_lossy(&refused.stderr).contains("worker is running"));
    stop(node, &cfg).await;
    ok(volume(&[&"export", &cfg.data_dir, &"counter", &"alice", &out]));

    let mut fresh = cfg.clone();
    fresh.data_dir = cfg.data_dir.parent().unwrap().join("fresh");
    ok(volume(&[&"import", &out, &fresh.data_dir]));
    let node = start(&fresh, &wasm);
    let got: serde_json::Value = serde_json::from_str(&call(&node, "GET", "alice", "/", "").await).unwrap();
    assert_eq!(got["count"], 3, "{got}");
    assert_eq!(call(&node, "GET", "alice", "/kv/greeting", "").await, "hello from the old root");
    call(&node, "POST", "alice", "/incr", "").await;
    stop(node, &fresh).await;
    let node = start(&fresh, &wasm);
    let got: serde_json::Value = serde_json::from_str(&call(&node, "GET", "alice", "/", "").await).unwrap();
    assert_eq!(got["count"], 4, "{got}");
    // Importing over it again is refused.
    assert!(!volume(&[&"import", &out, &fresh.data_dir]).status.success());
    stop(node, &fresh).await;
}

/// With local capture on, the newest committed version is an export: taken
/// while the node runs, without its lock, and imported like any other.
#[tokio::test(flavor = "multi_thread")]
async fn volume_export_takes_a_committed_version_while_running() {
    let mut cfg = node_cfg("committed");
    cfg.capture = Some(CaptureConfig::default());
    let wasm = std::fs::read(repo().join("guests/target/wasm32-wasip2/release/counter.wasm")).unwrap();
    let node = start(&cfg, &wasm);
    call(&node, "POST", "bob", "/incr", "").await;
    call(&node, "PUT", "bob", "/kv/k", "v1").await;
    let out = cfg.data_dir.parent().unwrap().join("bob.tar");
    let said = ok(volume(&[&"export", &cfg.data_dir, &"counter", &"bob", &out, &"--committed"]));
    assert!(said.contains("committed version 2"), "{said}");
    let history = cfg.app_dirs("counter")[0].join("~committed/bob");
    assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(history.join(format!("{:020}.tar", 2))).unwrap());

    let mut fresh = cfg.clone();
    fresh.data_dir = cfg.data_dir.parent().unwrap().join("fresh");
    ok(volume(&[&"import", &out, &fresh.data_dir, &"--object", &"carol"]));
    let node2 = start(&fresh, &wasm);
    let got: serde_json::Value = serde_json::from_str(&call(&node2, "GET", "carol", "/", "").await).unwrap();
    assert_eq!(got["count"], 1, "{got}");
    assert_eq!(call(&node2, "GET", "carol", "/kv/k", "").await, "v1");
    stop(node2, &fresh).await;
    stop(node, &cfg).await;
}

/// SQL, files and KV written by the composed SQLite client move together;
/// an export of an object with uncommitted changes is refused until the
/// node has rolled them back.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the separately built SQLite experiment (WASI SDK)"]
async fn sqlite_volume_export_import_round_trip() {
    let cfg = node_cfg("sqlite");
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/composed.wasm")).unwrap();
    let node = start(&cfg, &wasm);
    call(&node, "GET", "a", "/?v=first", "").await;
    call(&node, "GET", "a", "/?v=second", "").await;
    stop(node, &cfg).await;
    // A crash-left undo log: export must wait for recovery.
    let journal = cfg.app_dirs("counter")[0].join("~journal/a");
    std::fs::create_dir_all(&journal).unwrap();
    std::fs::write(journal.join("log"), b"").unwrap();
    let out = cfg.data_dir.parent().unwrap().join("a.tar");
    let refused = volume(&[&"export", &cfg.data_dir, &"counter", &"a", &out]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("uncommitted"), "{refused:?}");
    std::fs::remove_file(journal.join("log")).unwrap();
    ok(volume(&[&"export", &cfg.data_dir, &"counter", &"a", &out]));

    let mut fresh = cfg.clone();
    fresh.data_dir = cfg.data_dir.parent().unwrap().join("fresh");
    ok(volume(&[&"import", &out, &fresh.data_dir]));
    assert!(fresh.app_dirs("counter")[0].join("a/composed.db").is_file());
    let node = start(&fresh, &wasm);
    let got: serde_json::Value = serde_json::from_str(&call(&node, "GET", "a", "/", "").await).unwrap();
    assert_eq!(got["rows"], serde_json::json!(["first", "second"]), "{got}");
    assert_eq!((got["file"].as_str(), got["key"].as_str()), (Some("second"), Some("second")), "{got}");
    call(&node, "GET", "a", "/?v=third", "").await;
    stop(node, &fresh).await;
    let node = start(&fresh, &wasm);
    let got: serde_json::Value = serde_json::from_str(&call(&node, "GET", "a", "/", "").await).unwrap();
    assert_eq!(got["rows"], serde_json::json!(["first", "second", "third"]), "{got}");
    stop(node, &fresh).await;
}
