//! Bounded correctness stress for the locally composed hostile guest.
//!
//! Run from the repository: `bash -lc 'ulimit -n 100000; cargo run -p nanovm
//! --example kv_stress'`. Do not overlap runs with builds when comparing timings.
//! Append `-- --quota-only` to reproduce quota boundaries without the large fill.
//! Uses unique disposable data directories, retained on failure. Timings include
//! 10ms journal sampling and are diagnostics, not benchmark results. Peaks are
//! sampled lower bounds; open-but-unlinked files are not visible to this sampler.
//! The current guest list endpoint checks stream completion/count, not identities;
//! exact persisted key identities and values are checked separately on disk.

use std::{
    collections::BTreeSet,
    fs,
    io::ErrorKind,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use nanovm::{
    config::{AppConfig, NodeConfig},
    table::Node,
};
use serde_json::{Value, json};

#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;

#[derive(Default)]
struct Disk {
    bytes: u64,
    allocated: u64,
    files: u64,
    retained: u64,
}

fn disk(path: &Path) -> anyhow::Result<Disk> {
    let mut result = Disk::default();
    let mut stack = vec![path.to_path_buf()];
    while let Some(path) = stack.pop() {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let md = match entry.metadata() {
                Ok(md) => md,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if md.is_dir() {
                stack.push(entry.path());
            } else if md.is_file() {
                result.files += 1;
                result.bytes += md.len();
                result.allocated += md.blocks() * 512;
                if entry.file_name() != "log" {
                    result.retained += md.len();
                }
            }
        }
    }
    Ok(result)
}

fn volume(root: &Path, id: &str) -> PathBuf {
    nanovm::app::object_dir(&root.join("objects/stress"), id).join("data")
}

async fn request(node: &Arc<Node>, root: &Path, id: &str, path: &str) -> anyhow::Result<load::Reply> {
    let journal = nanovm::app::object_dir(&root.join("objects/stress"), id).join("journal");
    let mut peak = Disk::default();
    let mut samples = 0;
    let mut interval = tokio::time::interval(Duration::from_millis(10));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let reply = load::request(node, "GET", id, path, "");
    tokio::pin!(reply);
    let reply = loop {
        tokio::select! {
            reply = &mut reply => break reply,
            _ = interval.tick() => {
                let sample = disk(&journal)?;
                peak.bytes = peak.bytes.max(sample.bytes);
                peak.allocated = peak.allocated.max(sample.allocated);
                peak.retained = peak.retained.max(sample.retained);
                samples += 1;
            }
        }
    };
    let journal_at_reply = disk(&journal)?.bytes;
    // Host errors can reach the client before instance teardown/recovery. A
    // harmless queued request is the lifecycle barrier, not an arbitrary sleep.
    if reply.error.is_some() {
        let barrier = load::request(node, "GET", id, "/", "").await;
        ensure!(barrier.status == 200, "recovery barrier: {}", barrier.body);
    }
    let steady = disk(&volume(root, id))?;
    let journal_after = disk(&journal)?;
    println!(
        "{}",
        json!({"object":id,"path":path,"status":reply.status,"error":reply.error,"body":reply.body,
            "elapsed_ms":reply.elapsed.as_secs_f64()*1000.0,"samples":samples,
            "sampled_peak_journal_bytes":peak.bytes,"sampled_peak_retained_bytes":peak.retained,
            "sampled_peak_journal_allocated_bytes":peak.allocated,
            "steady_volume_bytes":steady.bytes,"steady_volume_files":steady.files,
            "steady_volume_allocated_bytes":steady.allocated,"journal_at_reply_bytes":journal_at_reply,
            "journal_after_recovery_bytes":journal_after.bytes})
    );
    ensure!(journal_after.bytes == 0, "journal remains after {id} {path}");
    Ok(reply)
}

async fn ok(node: &Arc<Node>, root: &Path, id: &str, path: &str) -> anyhow::Result<Value> {
    let reply = request(node, root, id, path).await?;
    ensure!(reply.status == 200, "{id} {path}: {} {}", reply.status, reply.body);
    serde_json::from_str(&reply.body).context("guest JSON response")
}

fn persisted_keys(root: &Path, id: &str, n: usize, size: usize) -> anyhow::Result<()> {
    let dir = volume(root, id).join(".kv/default");
    let actual: BTreeSet<_> = fs::read_dir(&dir)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<_, _>>()?;
    let expected: BTreeSet<_> = (0..n).map(|i| URL_SAFE_NO_PAD.encode(format!("k{i}")).into()).collect();
    ensure!(actual == expected, "persisted key set mismatch for {id}");
    for name in expected {
        ensure!(fs::read(dir.join(name))? == vec![7; size], "value mismatch for {id}");
    }
    Ok(())
}

async fn workloads(node: &Arc<Node>, root: &Path) -> anyhow::Result<()> {
    for n in [100, 1000, 10000] {
        let id = format!("keys-{n}");
        // Grow and shrink values through repeated atomic replacements.
        for size in [1, 32, 3] {
            let value = ok(node, root, &id, &format!("/kv?op=fill&n={n}&size={size}")).await?;
            ensure!(value == json!({"set":n,"error":null}), "fill: {value}");
            let value = ok(node, root, &id, "/kv?op=list").await?;
            ensure!(value == json!({"keys":n}), "streamed list: {value}");
            let value = ok(node, root, &id, "/kv?op=get").await?;
            ensure!(value == json!({"bytes":size}), "lookup: {value}");
            persisted_keys(root, &id, n, size)?;
        }
    }
    for _ in 0..8 {
        let value = ok(node, root, "replace", "/kv?op=fill&n=1&size=1048576").await?;
        ensure!(value == json!({"set":1,"error":null}), "replacement: {value}");
        persisted_keys(root, "replace", 1, 1048576)?;
    }
    let expected = json!({"file":"committed","key":"committed"});
    ensure!(ok(node, root, "rollback", "/commit?v=committed").await? == expected);
    // A handled HTTP error commits; only failure to answer rolls back.
    let reply = request(node, root, "rollback", "/commit?v=committed&then=fail").await?;
    ensure!(reply.status == 507, "handled error: {}", reply.body);
    for (mode, status) in [("panic", 500), ("error", 500)] {
        let reply = request(node, root, "rollback", &format!("/commit?v=discarded&then={mode}")).await?;
        ensure!(
            reply.status == status,
            "rollback {mode}: {} {}",
            reply.status,
            reply.body
        );
        ensure!(
            ok(node, root, "rollback", "/commit").await? == expected,
            "rollback {mode}"
        );
    }
    let value = ok(node, root, "collision", "/kv?op=plant").await?;
    ensure!(
        value.get("error").is_some(),
        "directory collision unexpectedly accepted: {value}"
    );
    ensure!(
        disk(&volume(root, "collision"))?.bytes == 0,
        "collision leaked staged value"
    );
    Ok(())
}

async fn boundaries(node: &Arc<Node>, root: &Path) -> anyhow::Result<()> {
    // The quota node has exactly 1 MiB and 16 entries per object.
    let value = ok(node, root, "bytes", "/fill?mount=data&mb=1").await?;
    ensure!(value == json!({"written":1048576}), "byte boundary: {value}");
    let reply = request(node, root, "bytes", "/fill?mount=data&mb=2").await?;
    ensure!(reply.status == 507, "over byte quota: {} {}", reply.status, reply.body);
    ensure!(fs::read(volume(root, "bytes").join("fill.bin"))? == vec![0xAB; 1 << 20]);
    let value = ok(node, root, "files", "/files?mount=data&n=16").await?;
    ensure!(value == json!({"created":16}), "file boundary: {value}");
    let reply = request(node, root, "files", "/files?mount=data&n=17").await?;
    ensure!(reply.status == 507, "over file quota: {} {}", reply.status, reply.body);
    ensure!(disk(&volume(root, "files"))?.files == 16, "file quota preservation");
    // A handled quota error commits the successful prefix, but cannot exceed the limit.
    let reply = request(node, root, "new-files", "/files?mount=data&n=17").await?;
    ensure!(reply.status == 507, "new files exceeded quota: {}", reply.body);
    ensure!(disk(&volume(root, "new-files"))?.files == 16, "new-file boundary");
    let value = ok(node, root, "kv-bytes", "/kv?op=fill&n=2&size=1048576").await?;
    ensure!(value["set"] == 1 && value["error"].is_string(), "KV quota: {value}");
    persisted_keys(root, "kv-bytes", 1, 1048576)?;
    let value = ok(node, root, "kv-files", "/kv?op=fill&n=20&size=1").await?;
    // .kv and default consume two entries; atomic staging needs one free entry.
    ensure!(value["error"].is_string(), "KV file quota: {value}");
    let count = value["set"].as_u64().context("set count")? as usize;
    ensure!(count > 0 && count <= 14, "KV file quota count: {count}");
    persisted_keys(root, "kv-files", count, 1)?;
    let value = ok(node, root, "kv-files", "/kv?op=list").await?;
    ensure!(value == json!({"keys":count}), "listing at file quota: {value}");
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let wasm = fs::read(repo.join("guests/target/wasm32-wasip2/release/hostile.wasm"))
        .context("build the locally composed hostile guest with scripts/build-guests.sh")?;
    let root = repo.join(format!(
        "target/kv-stress-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir(&root)?;
    println!("data retained on failure: {}", root.display());
    let cfg = NodeConfig {
        data_dir: root.join("large"),
        memory_reservation_mb: 64,
        max_live_objects: 16,
        ..Default::default()
    };
    let mut app = AppConfig {
        name: "stress".into(),
        digest: "local-hostile".into(),
        env: Default::default(),
        config: Default::default(),
        worker: Default::default(),
        limits: serde_json::from_value(json!({"memory_mb":64,"cpu_ms":60000,
            "request_timeout_ms":120000,"disk_mb":16,"max_files":12010,"max_resources":256}))?,
    };
    cfg.validate()?;
    nanovm::engine::preflight(&cfg)?;
    let quota_only = std::env::args().any(|arg| arg == "--quota-only");
    if !quota_only {
        let node = Node::new(cfg.clone(), &app, &wasm, None)?;
        let result = workloads(&node, &cfg.data_dir).await;
        node.shutdown().await;
        drop(node);
        result?;
        let node = Node::new(cfg.clone(), &app, &wasm, None)?;
        let result = async {
            for n in [100, 1000, 10000] {
                let id = format!("keys-{n}");
                ensure!(ok(&node, &cfg.data_dir, &id, "/kv?op=list").await? == json!({"keys":n}));
                ensure!(ok(&node, &cfg.data_dir, &id, "/kv?op=get").await? == json!({"bytes":3}));
                persisted_keys(&cfg.data_dir, &id, n, 3)?;
            }
            ensure!(
                ok(&node, &cfg.data_dir, "rollback", "/commit").await? == json!({"file":"committed","key":"committed"})
            );
            anyhow::Ok(())
        }
        .await;
        node.shutdown().await;
        drop(node);
        result?;
    }
    let cfg = NodeConfig {
        data_dir: root.join("quota"),
        ..cfg
    };
    app.limits.disk_mb = 1;
    app.limits.max_files = 16;
    let node = Node::new(cfg.clone(), &app, &wasm, None)?;
    let result = boundaries(&node, &cfg.data_dir).await;
    node.shutdown().await;
    drop(node);
    result?;
    fs::remove_dir_all(root)?;
    println!(
        "PASS {}",
        if quota_only {
            "quota boundaries"
        } else {
            "bounded KV stress, rollback/restart, byte/file quota boundaries"
        }
    );
    Ok(())
}
