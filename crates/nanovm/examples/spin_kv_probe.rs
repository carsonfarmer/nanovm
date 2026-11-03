//! Run the composed Spin KV prototype against the real transactional node.
use anyhow::{Context, ensure};
use nanovm::{
    config::{AppConfig, NodeConfig},
    table::Node,
};
use std::path::Path;
#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let wasm = std::fs::read(root.join("target/kv-filesystem/spin-composed.wasm"))
        .context("run bash experiments/kv-filesystem/build.sh")?;
    let dir = root.join(format!(
        "target/spin-kv-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let cfg = NodeConfig {
        data_dir: dir.clone(),
        memory_reservation_mb: 64,
        ..Default::default()
    };
    let app = AppConfig {
        name: "spin-kv".into(),
        digest: "probe".into(),
        env: Default::default(),
        config: Default::default(),
        worker: Default::default(),
        limits: serde_json::from_value(
            serde_json::json!({"memory_mb":64,"cpu_ms":10000,"request_timeout_ms":30000,"disk_mb":8,"max_resources":128}),
        )?,
    };
    cfg.validate()?;
    nanovm::engine::preflight(&cfg)?;
    let node = Node::new(cfg.clone(), &app, &wasm, None)?;
    for (path, expected) in [
        ("/set", "ok"),
        ("/get", "committed"),
        ("/contracts", "ok"),
        ("/populate", "ok"),
        ("/list", "ok"),
        ("/cancel", "ok"),
        ("/pressure", "ok"),
        ("/concurrent", "ok"),
        ("/empty", "ok"),
        ("/listing-error", "ok"),
        ("/set", "ok"),
    ] {
        let r = load::request(&node, "GET", "one", path, "").await;
        ensure!(r.status == 200 && r.body == expected, "{path}: {} {}", r.status, r.body);
        println!("PASS {path}");
    }
    let r = load::request(&node, "GET", "one", "/trap", "").await;
    ensure!(r.error.as_deref() == Some("trap"), "expected trap: {}", r.body);
    let r = load::request(&node, "GET", "one", "/get", "").await;
    ensure!(r.status == 200 && r.body == "committed", "rollback: {}", r.body);
    println!("PASS trap rollback");
    node.shutdown().await;
    drop(node);
    let node = Node::new(cfg, &app, &wasm, None)?;
    let r = load::request(&node, "GET", "one", "/get", "").await;
    ensure!(r.status == 200 && r.body == "committed", "restart: {}", r.body);
    println!("PASS restart persistence");
    let r = load::request(&node, "GET", "quota", "/quota", "").await;
    ensure!(
        r.status == 200 && r.body == "quota refused",
        "quota: {} {}",
        r.status,
        r.body
    );
    node.shutdown().await;
    drop(node);
    for id in ["one", "quota"] {
        for entry in std::fs::read_dir(dir.join(format!("objects/spin-kv/{id}/.kv")))? {
            ensure!(
                !entry?.file_name().to_string_lossy().starts_with(".spin-write-"),
                "temporary file leaked"
            );
        }
    }
    std::fs::remove_dir_all(dir)?;
    println!("PASS quota refusal and temporary cleanup");
    Ok(())
}
