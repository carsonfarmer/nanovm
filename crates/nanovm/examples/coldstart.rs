//! Cold-start costs of components, compared (the pre-initialization spike,
//! experiments/preinit). For each round and each component, alternating
//! their order every round:
//! - app cold start: `Node::new` with an empty compile cache (compiles), then
//!   again with the cache (deserializes), in process CPU;
//! - first request to a new object: `--objects` requests, each to an object
//!   never seen (instantiate, run, commit), then one more request to each
//!   (warm), in process CPU per request; and the private memory (resident,
//!   not shared: /proc/self/smaps_rollup) the live objects added, per object.
//! Prints each round, then medians and ranges.
//!
//!     cargo run -p nanovm --example coldstart -- [--rounds 5] [--objects 200]
//!         [--method GET] [--path /] NAME=app.wasm NAME=other.wasm ...

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context;
use nanovm::config::{AppConfig, NodeConfig};
use nanovm::table::Node;

#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;

/// This process's private resident memory, in KiB.
fn private_kib() -> f64 {
    let text = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
    text.lines()
        .filter(|l| l.starts_with("Private_Clean:") || l.starts_with("Private_Dirty:"))
        .filter_map(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok())
        .sum()
}

/// CPU this process has used, all threads, in microseconds.
fn cpu_us() -> f64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let t = |v: libc::timeval| v.tv_sec as f64 * 1e6 + v.tv_usec as f64;
    t(u.ru_utime) + t(u.ru_stime)
}

fn app() -> AppConfig {
    let limits = serde_json::json!({
        "memory_mb": 64, "cpu_ms": 5000, "request_timeout_ms": 30000, "idle_timeout_ms": 600000,
        "disk_mb": 8, "max_files": 1000,
    });
    AppConfig {
        name: "coldstart".into(),
        digest: "x".into(),
        env: Default::default(),
        config: Default::default(),
        limits: serde_json::from_value(limits).unwrap(),
        worker: Default::default(),
    }
}

fn cfg(dir: &Path, objects: usize, telemetry: &str) -> NodeConfig {
    let mut cfg = NodeConfig {
        data_dir: dir.into(),
        max_live_objects: objects + 8,
        memory_reservation_mb: 64,
        telemetry_endpoint: telemetry.into(),
        dev: true,
        ..NodeConfig::default()
    };
    cfg.limits.cpu_ms = 5000;
    cfg.limits.request_timeout_ms = 30000;
    cfg.limits.idle_timeout_ms = 600000;
    cfg
}

struct Sample {
    compile_ms: f64,
    cached_ms: f64,
    first_us: f64,
    warm_us: f64,
    first_wall_us: f64,
    private_kib: f64,
}

async fn measure(
    wasm: &[u8],
    dir: &Path,
    objects: usize,
    method: &str,
    path: &str,
    telemetry: &str,
) -> anyhow::Result<Sample> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir)?;
    let cfg = cfg(dir, objects, telemetry);
    nanovm::engine::preflight(&cfg)?;
    let c0 = cpu_us();
    let node = Node::new(cfg.clone(), &app(), wasm, None)?;
    let compile_ms = (cpu_us() - c0) / 1e3;
    node.shutdown().await;
    drop(node);
    // The final actor may hold the app's lock a moment longer.
    let c0 = cpu_us();
    let node = loop {
        match Node::new(cfg.clone(), &app(), wasm, None) {
            Ok(n) => break n,
            Err(e) if e.to_string().contains("already owned") => {
                std::thread::sleep(std::time::Duration::from_millis(1))
            }
            Err(e) => return Err(e),
        }
    };
    let cached_ms = (cpu_us() - c0) / 1e3;
    let run = async |round: &str| -> anyhow::Result<(f64, f64)> {
        let (c0, t0) = (cpu_us(), Instant::now());
        for i in 0..objects {
            let r = load::request(&node, method, &format!("{round}{i}"), path, "").await;
            anyhow::ensure!(r.status < 300, "{method} {path}: {} {:?} {}", r.status, r.error, r.body);
        }
        Ok((
            (cpu_us() - c0) / objects as f64,
            t0.elapsed().as_secs_f64() * 1e6 / objects as f64,
        ))
    };
    let before = private_kib();
    let (first_us, first_wall_us) = run("o").await?;
    let private_kib = (private_kib() - before) / objects as f64;
    let (warm_us, _) = run("o").await?;
    node.shutdown().await;
    Ok(Sample {
        compile_ms,
        cached_ms,
        first_us,
        warm_us,
        first_wall_us,
        private_kib,
    })
}

fn median_range(mut v: Vec<f64>) -> String {
    v.sort_by(f64::total_cmp);
    format!("{:.1} ({:.1}-{:.1})", v[v.len() / 2], v[0], v[v.len() - 1])
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (mut rounds, mut objects, mut method, mut path, mut variants) =
        (5, 200, "GET".to_string(), "/".to_string(), vec![]);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--rounds" => rounds = args.next().context("--rounds N")?.parse()?,
            "--objects" => objects = args.next().context("--objects N")?.parse()?,
            "--method" => method = args.next().context("--method M")?,
            "--path" => path = args.next().context("--path P")?,
            _ => {
                let (name, file) = a.split_once('=').context("NAME=app.wasm")?;
                variants.push((name.to_string(), std::fs::read(file).with_context(|| file.to_string())?));
            }
        }
    }
    // Telemetry goes to a sink that drops it, not to whatever is on 4318.
    let sink = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let telemetry = format!("http://{}", sink.local_addr()?);
    tokio::spawn(nanovm::server::serve_http1(
        sink,
        |req: http::Request<hyper::body::Incoming>| async move {
            use http_body_util::BodyExt;
            let _ = req.into_body().collect().await;
            http::Response::new(
                http_body_util::Full::new(bytes::Bytes::new())
                    .map_err(|e| match e {})
                    .boxed_unsync(),
            )
        },
    ));
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp/coldstart");
    let mut samples: Vec<Vec<Sample>> = variants.iter().map(|_| vec![]).collect();
    for r in 0..rounds {
        let mut order: Vec<usize> = (0..variants.len()).collect();
        if r % 2 == 1 {
            order.reverse();
        }
        for i in order {
            let (name, wasm) = &variants[i];
            let s = measure(wasm, &root.join(name), objects, &method, &path, &telemetry).await?;
            println!(
                "round {r} {name}: compile {:.1} ms, cached load {:.1} ms, first request {:.1} us CPU ({:.1} us wall), \
                 warm {:.1} us CPU, {:.0} KiB private per object",
                s.compile_ms, s.cached_ms, s.first_us, s.first_wall_us, s.warm_us, s.private_kib
            );
            samples[i].push(s);
        }
    }
    println!("\nmedian (min-max) over {rounds} rounds, {objects} objects each, {method} {path}:");
    println!(
        "{:<14} {:>10} {:>22} {:>22} {:>22} {:>22} {:>22}",
        "",
        "bytes",
        "compile ms CPU",
        "cached load ms CPU",
        "1st request us CPU",
        "warm request us CPU",
        "KiB private/object"
    );
    for ((name, wasm), s) in variants.iter().zip(&samples) {
        let col = |f: fn(&Sample) -> f64| median_range(s.iter().map(f).collect());
        println!(
            "{name:<14} {:>10} {:>22} {:>22} {:>22} {:>22} {:>22}",
            wasm.len(),
            col(|s| s.compile_ms),
            col(|s| s.cached_ms),
            col(|s| s.first_us),
            col(|s| s.warm_us),
            col(|s| s.private_kib)
        );
    }
    let _ = std::fs::remove_dir_all(root);
    Ok(())
}
