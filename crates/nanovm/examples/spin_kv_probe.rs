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
    let wasm =
        std::fs::read(root.join("target/keyvalue/tests.wasm")).context("run bash components/keyvalue/build.sh")?;
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
        secrets: Default::default(),
        worker: Default::default(),
        limits: serde_json::from_value(
            serde_json::json!({"memory_mb":64,"cpu_ms":10000,"request_timeout_ms":30000,"disk_mb":8,"max_resources":128}),
        )?,
    };
    cfg.validate()?;
    nanovm::engine::preflight(&cfg)?;
    let mut read_only = app.clone();
    read_only.limits.max_files = 0;
    let node = Node::new(cfg.clone(), &read_only, &wasm, None)?;
    let r = load::request(&node, "GET", "fresh", "/empty", "").await;
    ensure!(r.status == 200 && r.body == "ok", "zero-entry read: {}", r.body);
    node.shutdown().await;
    drop(node);
    ensure!(!dir.join("objects/spin-kv/fresh/.kv").exists());
    println!("PASS read-only bucket with zero file quota creates nothing");
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
        ("/held-completions", "ok"),
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
    for abort in [false, true] {
        let id = if abort { "repeat-abort" } else { "repeat-commit" };
        ensure!(load::request(&node, "GET", id, "/repeat-seed", "").await.status == 200);
        let writing = {
            let node = node.clone();
            tokio::spawn(async move {
                load::request(&node, "GET", id, if abort { "/repeat?trap=1" } else { "/repeat" }, "").await
            })
        };
        let ready = dir.join(format!("objects/spin-kv/{id}/repeat-ready"));
        while !ready.exists() {
            ensure!(
                !writing.is_finished(),
                "repeated write ended before inspection: {}",
                writing.await?.body
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let journal = dir.join(format!("objects/spin-kv/~journal/{id}"));
        let mut retained = 0;
        for entry in std::fs::read_dir(&journal)? {
            let entry = entry?;
            if entry.file_name() != "log" {
                retained += entry.metadata()?.len();
            }
        }
        ensure!(
            retained == 4,
            "repeated replacements retained {retained} bytes, not just the seed"
        );
        println!(
            "1000 replacements: undo {} bytes, retained {retained} bytes",
            std::fs::metadata(journal.join("log"))?.len()
        );
        let r = writing.await?;
        ensure!(
            if abort {
                r.error.as_deref() == Some("trap")
            } else {
                r.status == 200
            },
            "repeat: {}",
            r.body
        );
        let r = load::request(&node, "GET", id, "/repeat-get", "").await;
        // load::Reply converts binary to lossy UTF-8; inspect the file bytes independently.
        ensure!(r.status == 200, "read after repeats: {}", r.body);
        let expected = if abort { b"seed".to_vec() } else { vec![246; 8193] };
        ensure!(std::fs::read(dir.join(format!("objects/spin-kv/{id}/.kv/default/cmVwZWF0")))? == expected);
    }
    node.shutdown().await;
    drop(node);
    let node = Node::new(cfg, &app, &wasm, None)?;
    let r = load::request(&node, "GET", "one", "/get", "").await;
    ensure!(r.status == 200 && r.body == "committed", "restart: {}", r.body);
    println!("PASS restart persistence");
    let r = load::request(&node, "GET", "collision", "/collision", "").await;
    ensure!(r.status == 200, "collision: {}", r.body);
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
        for entry in std::fs::read_dir(dir.join(format!("objects/spin-kv/{id}/.kv/default")))? {
            ensure!(
                !entry?.file_name().to_string_lossy().starts_with(".spin-write-"),
                "temporary file leaked"
            );
        }
    }
    std::fs::remove_dir_all(dir)?;
    println!("PASS quota refusal and temporary cleanup");
    if std::env::args().any(|arg| arg == "--test-hooks") {
        let wasm = std::fs::read(root.join("target/keyvalue/tests-faults.wasm"))?;
        let cfg = NodeConfig {
            data_dir: root.join("target/kv-cancel-probe"),
            memory_reservation_mb: 64,
            ..Default::default()
        };
        let _ = std::fs::remove_dir_all(&cfg.data_dir);
        let node = Node::new(cfg.clone(), &app, &wasm, None)?;
        let r = load::request(&node, "GET", "cancel", "/cancel-write", "").await;
        ensure!(r.status == 200, "cancel-write: {}", r.body);
        node.shutdown().await;
        drop(node);
        for stage in ["created", "partial", "written", "renamed"] {
            let node = Node::new(cfg.clone(), &app, &wasm, None)?;
            ensure!(load::request(&node, "GET", "cut", "/set", "").await.status == 200);
            node.shutdown().await;
            drop(node);
            let mut armed = app.clone();
            armed.config.insert("kv-cut".into(), stage.into());
            let node = Node::new(cfg.clone(), &armed, &wasm, None)?;
            let r = load::request(&node, "GET", "cut", "/fault", "").await;
            ensure!(
                r.error.as_deref() == Some("trap") && r.body.contains("KV recovery cut"),
                "{stage}: {}",
                r.body
            );
            node.shutdown().await;
            drop(node);
            let node = Node::new(cfg.clone(), &app, &wasm, None)?;
            let r = load::request(&node, "GET", "cut", "/get", "").await;
            ensure!(r.status == 200 && r.body == "committed", "{stage}: {}", r.body);
            let volume = cfg.data_dir.join("objects/spin-kv/cut");
            ensure!(!volume.join("mixed").exists(), "mixed file survived {stage}");
            for entry in std::fs::read_dir(volume.join(".kv/default"))? {
                ensure!(
                    !entry?.file_name().to_string_lossy().starts_with(".spin-write-"),
                    "temporary survived {stage}"
                );
            }
            node.shutdown().await;
            drop(node);
            println!("PASS recovery cut {stage}");
        }
        std::fs::remove_dir_all(cfg.data_dir)?;
        println!("PASS started write cancellation");
    }
    Ok(())
}
