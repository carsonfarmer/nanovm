//! Matched HTTP-level KV workloads for native and composed providers.
use anyhow::ensure;
use nanovm::{
    config::{AppConfig, NodeConfig},
    table::Node,
};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let wasm = std::fs::read(std::env::args().nth(1).expect("counter wasm path"))?;
    let unique = std::env::args().any(|arg| arg == "--unique");
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
        let mut value = "x".repeat(size);
        let r = load::request(&node, "PUT", "warm", "/kv/key", &value).await;
        ensure!(r.status == 204, "warm: {} {}", r.status, r.body);
        for (method, count) in [("GET", 1000), ("PUT", 200)] {
            let mut elapsed = Duration::ZERO;
            for sequence in 0..count {
                // Change every page, with little repeated content. Generation is
                // outside request timing, identical for both backend controls.
                if unique && method == "PUT" {
                    let mut seed = sequence as u64 + 1;
                    value = (0..size)
                        .map(|_| {
                            seed ^= seed << 13;
                            seed ^= seed >> 7;
                            seed ^= seed << 17;
                            char::from(b'!' + (seed % 90) as u8)
                        })
                        .collect();
                }
                let start = Instant::now();
                let r = load::request(
                    &node,
                    method,
                    "warm",
                    "/kv/key",
                    if method == "PUT" { &value } else { "" },
                )
                .await;
                elapsed += start.elapsed();
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
                elapsed.as_secs_f64() * 1000.0 / count as f64
            );
        }
        let r = load::request(&node, "GET", "warm", "/kv/key", "").await;
        ensure!(r.status == 200 && r.body == value, "final write mismatch");
    }
    let start = Instant::now();
    for n in 0..100 {
        let r = load::request(&node, "PUT", &format!("cold-{n}"), "/kv/key", "cold").await;
        ensure!(r.status == 204, "cold: {} {}", r.status, r.body);
    }
    println!("cold,4,100,{:.3}", start.elapsed().as_secs_f64() * 10.0);
    let start = Instant::now();
    for n in 0..100 {
        let r = load::request(&node, "GET", &format!("read-only-{n}"), "/kv/missing", "").await;
        ensure!(r.status == 404, "cold read: {} {}", r.status, r.body);
    }
    println!("cold-read,0,100,{:.3}", start.elapsed().as_secs_f64() * 10.0);
    if std::env::args().any(|arg| arg == "--density") {
        let before = nanovm::metrics::rss_bytes();
        let value = "x".repeat(1048576);
        for i in 0..32 {
            let r = load::request(&node, "PUT", &format!("large-{i}"), "/kv/key", &value).await;
            ensure!(r.status == 204, "large object: {} {}", r.status, r.body);
            let r = load::request(&node, "GET", &format!("large-{i}"), "/kv/key", "").await;
            ensure!(r.status == 200 && r.body == value, "large object read mismatch");
        }
        println!(
            "large-object-rss-kib,32,{:.1}",
            nanovm::metrics::rss_bytes().saturating_sub(before) as f64 / 32.0 / 1024.0
        );
    }
    node.shutdown().await;
    drop(node);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}
