//! Matched HTTP-level KV workloads for native and composed providers.
use anyhow::ensure;
use nanovm::{
    config::{AppConfig, NodeConfig},
    table::Node,
};
use std::{path::PathBuf, time::Instant};
#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let wasm = std::fs::read(std::env::args().nth(1).expect("counter wasm path"))?;
    let dir = PathBuf::from(format!("/tmp/nanovm-kv-bench-{}", std::process::id()));
    let cfg = NodeConfig {
        data_dir: dir.clone(),
        memory_reservation_mb: 64,
        max_live_objects: 256,
        ..Default::default()
    };
    let app = AppConfig {
        name: "bench".into(),
        digest: "bench".into(),
        env: Default::default(),
        config: Default::default(),
        worker: Default::default(),
        limits: serde_json::from_value(
            serde_json::json!({"memory_mb":32,"cpu_ms":5000,"request_timeout_ms":10000,"disk_mb":16,"max_files":10000}),
        )?,
    };
    nanovm::engine::preflight(&cfg)?;
    let node = Node::new(cfg, &app, &wasm, None)?;
    for size in [128, 16384, 1048576] {
        let value = "x".repeat(size);
        let r = load::request(&node, "PUT", "warm", "/kv/key", &value).await;
        ensure!(r.status == 204, "warm: {} {}", r.status, r.body);
        for (method, count) in [("GET", 1000), ("PUT", 200)] {
            let start = Instant::now();
            for _ in 0..count {
                let r = load::request(
                    &node,
                    method,
                    "warm",
                    "/kv/key",
                    if method == "PUT" { &value } else { "" },
                )
                .await;
                ensure!(
                    r.status == if method == "GET" { 200 } else { 204 },
                    "{} {}",
                    r.status,
                    r.body
                );
                if method == "GET" {
                    ensure!(r.body == value, "read mismatch");
                }
            }
            println!(
                "{method},{size},{count},{:.3}",
                start.elapsed().as_secs_f64() * 1000.0 / count as f64
            );
        }
    }
    let start = Instant::now();
    for n in 0..100 {
        let r = load::request(&node, "PUT", &format!("cold-{n}"), "/kv/key", "cold").await;
        ensure!(r.status == 204, "cold: {} {}", r.status, r.body);
    }
    println!("cold,4,100,{:.3}", start.elapsed().as_secs_f64() * 10.0);
    node.shutdown().await;
    drop(node);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}
