//! Execute the W-18 composition ABI gate; not a production KV implementation.
use anyhow::{Context, ensure};
use nanovm::config::{AppConfig, NodeConfig};
use nanovm::table::Node;
use std::path::Path;

#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let wasm = std::fs::read(root.join("target/kv-filesystem/composed.wasm"))
        .context("run experiments/kv-filesystem/build.sh first")?;
    let dir = root.join(format!(
        "target/kv-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    std::fs::create_dir(&dir)?;
    let cfg = NodeConfig {
        data_dir: dir.clone(),
        memory_reservation_mb: 64,
        ..Default::default()
    };
    let app = AppConfig {
        name: "kv-probe".into(),
        digest: "probe".into(),
        env: Default::default(),
        config: Default::default(),
        worker: Default::default(),
        limits: serde_json::from_value(
            serde_json::json!({"memory_mb":64,"cpu_ms":2000,"request_timeout_ms":5000,"disk_mb":8}),
        )?,
    };
    cfg.validate()?;
    nanovm::engine::preflight(&cfg)?;
    let node = Node::new(cfg, &app, &wasm, None)?;
    for (path, expected) in [("/control", "W18 composed provider"), ("/direct", "filesystem-control")] {
        let response = load::request(&node, "GET", "one", path, "").await;
        ensure!(
            response.status == 200 && response.body.contains(expected),
            "{path}: {} {}",
            response.status,
            response.body
        );
        println!("PASS {path}: {}", response.body);
    }
    for path in ["/read", "/write"] {
        let response = load::request(&node, "GET", "one", path, "").await;
        println!("{path}: {} {}", response.status, response.body);
        ensure!(
            response.error.as_deref() == Some("trap") && response.body.contains("cannot block a synchronous task"),
            "expected synchronous ABI suspension failure at {path}, got {}",
            response.body
        );
    }
    // Recovery runs when the next instance opens the volume, not at error headers.
    let recovered = load::request(&node, "GET", "one", "/control", "").await;
    ensure!(recovered.status == 200, "instance recovery failed: {}", recovered.body);
    let path = dir.join("objects/kv-probe/one/.kv/default/aw");
    ensure!(
        std::fs::read(path)? == b"filesystem-control",
        "failed writes must not change committed data"
    );
    node.shutdown().await;
    drop(node);
    std::fs::remove_dir_all(dir)?;
    println!("PASS: composed sync KV reaches provider but cannot suspend for P3 filesystem I/O");
    Ok(())
}
