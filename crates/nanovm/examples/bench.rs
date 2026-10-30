//! In-process benchmarks: cold starts, warm throughput, kill-and-recover,
//! churn past capacity, and neighbour latency under attack.
//!
//!     scripts/build-guests.sh
//!     cargo run --release --example bench [-- --quick] [-- --json out.json] [-- --cold N]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

use anyhow::Context;
use nanovm::config::{AppConfig, NodeConfig};
#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;

use load::{Reply, Run, request};
use nanovm::metrics::rss_bytes;
use nanovm::table::Node;
use serde_json::{Value, json};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// An in-process node running the guest `app`, with its own data directory.
fn node(name: &str, app: &str, max_live: usize) -> anyhow::Result<Arc<Node>> {
    let root = repo().join("target/bench-data");
    let dir = root.join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::create_dir_all(root.join("cache"))?;
    std::os::unix::fs::symlink(root.join("cache"), dir.join("cache"))?;
    let guest = repo().join(format!("guests/target/wasm32-wasip2/release/{app}.wasm"));
    let wasm = std::fs::read(&guest).with_context(|| format!("{}: run scripts/build-guests.sh", guest.display()))?;
    let limits = match app {
        "counter" => json!({ "memory_mb": 16, "cpu_ms": 50, "idle_timeout_ms": 600000, "disk_mb": 1, "max_queue": 1024 }),
        _ => json!({ "memory_mb": 32, "cpu_ms": 100, "request_timeout_ms": 1000, "max_resources": 2000 }),
    };
    let app = AppConfig {
        name: app.into(),
        env: Default::default(),
        config: Default::default(),
        limits: serde_json::from_value(limits)?,
        worker: Default::default(),
    };
    let cfg = NodeConfig {
        data_dir: dir,
        max_live_objects: max_live,
        memory_reservation_mb: 64,
        ..NodeConfig::default()
    };
    cfg.validate()?;
    nanovm::engine::preflight(&cfg)?;
    Node::new(cfg, &app, &wasm, None)
}

async fn once(node: &Arc<Node>, id: &str, path: &str) -> Reply {
    request(node, "GET", id, path, "").await
}

async fn load(
    node: &Arc<Node>,
    method: &str,
    path: &str,
    n: usize,
    conc: usize,
    id: impl Fn(usize) -> String + Send + Sync + 'static,
) -> Run {
    load::run(node, method, path, n, None, conc, id).await
}

fn row(name: &str, r: &Run, note: &str) {
    println!(
        "  {name:<34} {:>9.0}/s   p50 {:>7.2} ms   p99 {:>7.2} ms   p99.9 {:>7.2} ms   {}{note}",
        r.rps(),
        r.pct(0.5),
        r.pct(0.99),
        r.pct(0.999),
        if r.failed() > 0 {
            format!("FAILED {} {:?}  ", r.failed(), r.failures)
        } else {
            String::new()
        },
    );
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let quick = args.iter().any(|a| a == "--quick");
    let json_out = args
        .iter()
        .position(|a| a == "--json")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let scale = if quick { 10 } else { 1 };
    // `--cold N` overrides the cold-start count, for hosts whose fd limit
    // can't hold 10k live objects (see `engine::preflight`).
    let cold_n = args
        .iter()
        .position(|a| a == "--cold")
        .and_then(|i| args.get(i + 1))
        .map(|v| v.parse().expect("--cold takes a number"))
        .unwrap_or(10_000 / scale);
    let warm_objects = 1_000 / scale;
    let warm_n = 500_000 / scale;
    let conc = 256;
    let mut out = serde_json::Map::new();

    println!(
        "nanovm bench · {} {} · {} cores{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism().map_or(0, |n| n.get()),
        if quick { " · --quick" } else { "" }
    );

    // ---- cold starts, then warm traffic on the same node ----
    let n = node("main", "counter", cold_n + 64)?;
    once(&n, "warmup", "/").await;
    let rss0 = rss_bytes();
    let cold = load(&n, "GET", "/", cold_n, conc, |i| format!("o{i}")).await;
    let live = n.live_objects();
    let rss1 = rss_bytes();
    let per_obj_kb = rss1.saturating_sub(rss0) as f64 / live.max(1) as f64 / 1024.0;
    let inst = n.metrics.instantiate.quantile(0.5);
    println!("\ncold start ({cold_n} new objects, {conc} concurrent)");
    row("first request to a new object", &cold, "");
    println!(
        "  {:<34} {live} live · RSS {:.0} MB · {per_obj_kb:.0} KB/object · instantiate p50 ≤{} µs",
        "",
        rss1 as f64 / 1e6,
        inst
    );
    out.insert(
        "cold".into(),
        json!({ "run": cold.json(), "live": live, "rss_mb": rss1 as f64 / 1e6, "kb_per_object": per_obj_kb.round() }),
    );

    let warm = load(&n, "GET", "/", warm_n, conc, move |i| {
        format!("o{}", i % warm_objects)
    })
    .await;
    let serial = load(&n, "GET", "/", 20_000 / scale, 1, |_| "o0".into()).await;
    let incr = load(&n, "POST", "/incr", warm_n / 5, conc, move |i| {
        format!("o{}", i % warm_objects)
    })
    .await;
    let incr1 = load(&n, "POST", "/incr", 2_000 / scale, 1, |_| "o0".into()).await;
    println!("\nwarm ({warm_objects} live objects)");
    row(&format!("GET, {conc} concurrent"), &warm, "");
    row("GET, one client, one object", &serial, "");
    row(&format!("POST /incr (writes /data), {conc} conc"), &incr, "");
    row("POST /incr, one client, one object", &incr1, "");
    out.insert("warm_get".into(), warm.json());
    out.insert("warm_serial".into(), serial.json());
    out.insert("warm_incr".into(), incr.json());
    out.insert("warm_incr_serial".into(), incr1.json());
    n.shutdown().await;
    drop(n);

    // ---- kill and recover ----
    let n = node("kill", "hostile", 256)?;
    let kills = 300 / scale;
    let t0 = Instant::now();
    let mut recover = Run::default();
    for i in 0..kills {
        let id = format!("k{}", i % 16);
        once(&n, &id, "/panic").await;
        recover.add(&once(&n, &id, "/").await);
    }
    recover.elapsed = t0.elapsed();
    recover.lat_us.sort_unstable();
    println!("\nkill and recover ({kills} traps, serial)");
    row("next request after a trap", &recover, "(fresh instance)");
    out.insert("recover".into(), recover.json());
    n.shutdown().await;
    drop(n);

    // ---- churn past capacity: every request evicts an idle object ----
    // `--churn-cap N` sets the slot count, to measure eviction near a large cap.
    let cap = args
        .iter()
        .position(|a| a == "--churn-cap")
        .and_then(|i| args.get(i + 1))
        .map(|v| v.parse().expect("--churn-cap takes a number"))
        .unwrap_or(1_000 / scale);
    let n = node("churn", "counter", cap)?;
    let churn = load(&n, "GET", "/", 20_000 / scale, 64, |i| format!("c{i}")).await;
    println!(
        "\nchurn ({} new objects through {cap} slots, 64 concurrent)",
        20_000 / scale
    );
    row("new object + LRU eviction", &churn, "");
    for e in n
        .metrics
        .events_since(0)
        .iter()
        .filter(|e| e.kind == "instantiate-failed")
        .take(3)
    {
        println!("    instantiate-failed: {}", e.detail);
    }
    println!("  {:<34} {} evicted", "", n.metrics.evicted_lru.load(Relaxed));
    out.insert("churn".into(), churn.json());
    n.shutdown().await;
    drop(n);

    // ---- neighbours under attack: other objects of the attackers' app ----
    let n = node("attack", "hostile", 1_000)?;
    let objs = 100 / scale.min(10);
    load(&n, "GET", "/", objs, 64, |i| format!("v{i}")).await;
    let window = Duration::from_millis(if quick { 1000 } else { 3000 });
    let base = load::run(
        &n,
        "GET",
        "/",
        usize::MAX,
        Some(Instant::now() + window),
        64,
        move |i| format!("v{}", i % objs),
    )
    .await;
    let stop = Arc::new(AtomicBool::new(false));
    let attackers: Vec<_> = (0..16)
        .map(|a| {
            let (n, stop) = (n.clone(), stop.clone());
            tokio::spawn(async move {
                let paths = ["/spin", "/alloc?mb=1024", "/panic", "/leak"];
                let mut k = 0;
                while !stop.load(Relaxed) {
                    once(&n, &format!("a{a}"), paths[k % paths.len()]).await;
                    k += 1;
                }
                k
            })
        })
        .collect();
    // Let the attackers get going before measuring.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let killed0: u64 = n.metrics.poisons.iter().map(|p| p.load(Relaxed)).sum();
    let under = load::run(
        &n,
        "GET",
        "/",
        usize::MAX,
        Some(Instant::now() + window),
        64,
        move |i| format!("v{}", i % objs),
    )
    .await;
    let poisons = n.metrics.poisons.iter().map(|p| p.load(Relaxed)).sum::<u64>() - killed0;
    stop.store(true, Relaxed);
    let mut attacks = 0;
    for a in attackers {
        attacks += a.await?;
    }
    println!(
        "\nneighbours ({objs} objects, 64 concurrent, {} s each)",
        window.as_secs_f64()
    );
    row("baseline", &base, "");
    row(
        "while 16 attackers loop",
        &under,
        &format!("({attacks} attacks, {poisons} instances killed)"),
    );
    out.insert(
        "neighbours".into(),
        json!({ "baseline": base.json(), "under_attack": under.json(), "attacks": attacks, "killed": poisons }),
    );

    if let Some(path) = json_out {
        std::fs::write(&path, serde_json::to_string_pretty(&Value::Object(out))?)?;
        println!("\nwrote {path}");
    }
    Ok(())
}
