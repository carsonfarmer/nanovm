//! A/B of JavaScript apps against the Rust counter guest: cold start, warm
//! serial requests, writes, and memory per live object. Variants run
//! alternately, round after round; CPU time per request (the whole
//! process's user+sys over the phase) is the number to trust on a busy
//! machine.
//!
//!     scripts/build-guests.sh
//!     cargo run --release --example js-bench -- [--rounds 3] [--cold 200] [--warm 3000] \
//!         [name=path.wasm …]
//!
//! Without `name=path` arguments it compares `guests/…/counter.wasm` with
//! `js/target/counter.wasm` (js/examples/counter.js). Both serve
//! `GET /` (read the count) and `POST /incr` (write it).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nanovm::config::{AppConfig, NodeConfig};
use nanovm::metrics::rss_bytes;
use nanovm::table::Node;

#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;
use load::{Run, run};

#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// Process CPU time (user + sys).
fn cpu() -> Duration {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(u.ru_utime) + tv(u.ru_stime)
}

fn node(name: &str, wasm: &[u8], max_live: usize) -> anyhow::Result<(Arc<Node>, Duration)> {
    let root = repo().join("target/js-bench");
    let dir = root.join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::create_dir_all(root.join("cache"))?;
    std::os::unix::fs::symlink(root.join("cache"), dir.join("cache"))?;
    let limits = serde_json::json!({
        "memory_mb": 64, "cpu_ms": 1000, "idle_timeout_ms": 600000, "disk_mb": 1, "max_queue": 1024,
    });
    let app = AppConfig {
        name: name.into(),
        digest: "bench".into(),
        env: Default::default(),
        config: Default::default(),
        limits: serde_json::from_value(limits)?,
        worker: Default::default(),
    };
    let cfg = NodeConfig {
        data_dir: dir,
        max_live_objects: max_live,
        memory_reservation_mb: 128,
        ..NodeConfig::default()
    };
    cfg.validate()?;
    nanovm::engine::preflight(&cfg)?;
    let t0 = Instant::now();
    let node = Node::new(cfg, &app, wasm, None)?;
    Ok((node, t0.elapsed()))
}

/// One phase: its run and the CPU it cost per request.
async fn phase(node: &Arc<Node>, method: &str, path: &str, n: usize, id: impl Fn(usize) -> String + Send + Sync + 'static) -> (Run, Duration) {
    let c0 = cpu();
    let r = run(node, method, path, n, None, 1, id).await;
    assert_eq!(r.failed(), 0, "{method} {path}: {:?}", r.failures);
    (r, (cpu() - c0) / n as u32)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str, default: usize| {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map_or(default, |v| v.parse().unwrap())
    };
    let (rounds, cold, warm) = (flag("--rounds", 3), flag("--cold", 200), flag("--warm", 3000));
    let mut variants: Vec<(String, PathBuf)> = args
        .iter()
        .filter_map(|a| a.split_once('='))
        .map(|(n, p)| (n.to_string(), PathBuf::from(p)))
        .collect();
    if variants.is_empty() {
        variants = vec![
            ("rust".into(), repo().join("guests/target/wasm32-wasip2/release/counter.wasm")),
            ("js".into(), repo().join("js/target/counter.wasm")),
        ];
    }
    let wasms: Vec<Vec<u8>> = variants.iter().map(|(_, p)| std::fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).collect();
    println!("rounds {rounds} · {cold} cold starts · {warm} warm GETs, {} POSTs · serial, one client", warm / 5);
    println!("{:<8} {:>9} {:>22} {:>22} {:>22} {:>10}", "", "load ms", "cold p50/p99 · cpu", "GET p50/p99 · cpu", "POST p50/p99 · cpu", "KB/object");
    for round in 0..rounds {
        for ((name, _), wasm) in variants.iter().zip(&wasms) {
            let (node, load) = node(name, wasm, cold + 8)?;
            run(&node, "GET", "/", 1, None, 1, |_| "warmup".into()).await;
            let rss0 = rss_bytes();
            let (c, c_cpu) = phase(&node, "GET", "/", cold, |i| format!("c{i}")).await;
            let kb = (rss_bytes().saturating_sub(rss0)) as f64 / cold as f64 / 1024.0;
            let (g, g_cpu) = phase(&node, "GET", "/", warm, |_| "w".into()).await;
            let (p, p_cpu) = phase(&node, "POST", "/incr", warm / 5, |_| "w".into()).await;
            let cell = |r: &Run, cpu: Duration| format!("{:.2}/{:.2} · {:.0}µs", r.pct(0.5), r.pct(0.99), cpu.as_secs_f64() * 1e6);
            println!(
                "{:<8} {:>9.0} {:>22} {:>22} {:>22} {:>10.0}   (round {})",
                name,
                load.as_secs_f64() * 1e3,
                cell(&c, c_cpu),
                cell(&g, g_cpu),
                cell(&p, p_cpu),
                kb,
                round + 1
            );
            node.shutdown().await;
        }
    }
    Ok(())
}
