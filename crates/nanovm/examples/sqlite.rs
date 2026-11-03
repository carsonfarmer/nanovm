//! Run the composed SQLite example on a real in-process Node, without a registry.
//! Build with components/sqlite/build.sh, then cargo run --locked --example sqlite.
use std::path::Path;

use anyhow::{Context, ensure};
use nanovm::config::{AppConfig, NodeConfig};
use nanovm::table::Node;
use serde_json::json;

#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let wasm = std::fs::read(root.join("target/sqlite/example.wasm"))
        .context("first run components/sqlite/build.sh with WASI_SDK_PATH set")?;
    // Never delete an earlier run's state; retain failed runs for inspection.
    let dir = root.join(format!(
        "target/sqlite-example-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    std::fs::create_dir(&dir)?;
    let cfg = NodeConfig {
        data_dir: dir.clone(),
        max_live_objects: 2,
        memory_reservation_mb: 64,
        ..NodeConfig::default()
    };
    let app = AppConfig {
        name: "sqlite-example".into(),
        digest: "example".into(),
        env: Default::default(),
        config: Default::default(),
        limits: serde_json::from_value(json!({
            "memory_mb": 64, "cpu_ms": 2000, "request_timeout_ms": 5000,
            "disk_mb": 8, "max_files": 100
        }))?,
        worker: Default::default(),
    };
    cfg.validate()?;
    nanovm::engine::preflight(&cfg)?;
    let node = Node::new(cfg.clone(), &app, &wasm, None)?;
    let committed = json!({"sql": "hello SQLite", "file": "hello SQLite", "kv": "hello SQLite"});
    let empty = json!({"sql": null, "file": null, "kv": null});
    for (method, id, path, body, expected) in [
        ("GET", "alice", "/", "", &empty),
        ("POST", "alice", "/set", "hello SQLite", &committed),
        ("GET", "alice", "/", "", &committed),
        ("GET", "bob", "/", "", &empty),
    ] {
        let r = load::request(&node, method, id, path, body).await;
        ensure!(
            r.status == 200 && r.json() == *expected,
            "{method} {id}{path}: {} {}",
            r.status,
            r.body
        );
        println!("{method} {id}{path}: {}", r.body);
    }
    let failed = load::request(&node, "POST", "alice", "/set?rollback=1", "discard me").await;
    ensure!(
        failed.error.as_deref() == Some("guest-error"),
        "expected guest error, got {}",
        failed.body
    );
    let recovered = load::request(&node, "GET", "alice", "/", "").await;
    ensure!(
        recovered.status == 200 && recovered.json() == committed,
        "rollback: {}",
        recovered.body
    );
    println!("After intentional guest error: {}", recovered.body);
    node.shutdown().await;
    drop(node);
    let node = Node::new(cfg, &app, &wasm, None)?;
    let restarted = load::request(&node, "GET", "alice", "/", "").await;
    ensure!(
        restarted.status == 200 && restarted.json() == committed,
        "restart: {}",
        restarted.body
    );
    println!("After node restart: {}", restarted.body);
    node.shutdown().await;
    drop(node);
    std::fs::remove_dir_all(dir)?;
    println!("PASS: composed SQLite persistence, object isolation, mixed SQL/file/KV rollback, restart");
    Ok(())
}
