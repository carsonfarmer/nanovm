//! In-process benchmarks: cold starts, warm throughput, kill-and-recover,
//! churn past capacity, and neighbour latency under attack.
//!
//!     scripts/build-guests.sh
//!     cargo run --release --example bench [-- --quick] [-- --json out.json]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::BodyExt;
use nanovm::config::NodeConfig;
use nanovm::metrics::rss_bytes;
use nanovm::table::Node;
use serde_json::{Value, json};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn node(name: &str, max_live: usize) -> anyhow::Result<Arc<Node>> {
    let root = repo().join("target/bench-data");
    let dir = root.join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::create_dir_all(root.join("cache"))?;
    std::os::unix::fs::symlink(root.join("cache"), dir.join("cache"))?;
    let guest = |g: &str| repo().join(format!("guests/target/wasm32-wasip2/release/{g}.wasm"));
    for g in ["counter", "hostile"] {
        anyhow::ensure!(guest(g).exists(), "{} is missing: run scripts/build-guests.sh", guest(g).display());
    }
    let text = format!(
        r#"
        data_dir = "{dir}"
        max_live_objects = {max_live}
        memory_reservation_mb = 64

        [[app]]
        name = "counter"
        component = "{counter}"
        [app.limits]
        memory_mb = 16
        cpu_ms = 50
        idle_timeout_ms = 600000
        data_mb = 1
        tmp_mb = 1
        max_queue = 1024

        [[app]]
        name = "hostile"
        component = "{hostile}"
        [app.limits]
        memory_mb = 32
        cpu_ms = 100
        request_timeout_ms = 1000
        max_resources = 2000
        "#,
        dir = dir.display(),
        counter = guest("counter").display(),
        hostile = guest("hostile").display(),
    );
    let cfg: NodeConfig = toml::from_str(&text)?;
    cfg.validate()?;
    Node::new(cfg)
}

async fn once(node: &Arc<Node>, app: &str, id: &str, method: &str, path: &str) -> u16 {
    let req = http::Request::builder()
        .method(method)
        .uri(format!("http://localhost{path}"))
        .body(Bytes::new())
        .unwrap();
    let resp = node.dispatch(app, id, req).await;
    let status = resp.status().as_u16();
    let _ = resp.into_body().collect().await;
    status
}

struct Run {
    elapsed: Duration,
    /// Per-request latency in µs, sorted.
    lat: Vec<u32>,
    failed: usize,
}

impl Run {
    fn rps(&self) -> f64 {
        self.lat.len() as f64 / self.elapsed.as_secs_f64()
    }

    fn pct(&self, q: f64) -> f64 {
        let i = ((self.lat.len() as f64 * q).ceil() as usize).clamp(1, self.lat.len()) - 1;
        self.lat[i] as f64 / 1000.0
    }

    fn json(&self) -> Value {
        json!({
            "requests": self.lat.len(),
            "failed": self.failed,
            "elapsed_ms": self.elapsed.as_secs_f64() * 1e3,
            "rps": self.rps().round(),
            "p50_ms": self.pct(0.50),
            "p99_ms": self.pct(0.99),
            "p999_ms": self.pct(0.999),
        })
    }
}

/// `n` requests from `conc` workers; request `i` goes to object `id(i)`.
async fn load(
    node: &Arc<Node>,
    app: &'static str,
    method: &'static str,
    path: &'static str,
    n: usize,
    conc: usize,
    id: impl Fn(usize) -> String + Send + Sync + 'static,
) -> Run {
    load_until(node, app, method, path, n, None, conc, id).await
}

/// Like `load`, but stops at `deadline` if that comes first.
#[allow(clippy::too_many_arguments)]
async fn load_until(
    node: &Arc<Node>,
    app: &'static str,
    method: &'static str,
    path: &'static str,
    n: usize,
    deadline: Option<Instant>,
    conc: usize,
    id: impl Fn(usize) -> String + Send + Sync + 'static,
) -> Run {
    let next = Arc::new(AtomicUsize::new(0));
    let id = Arc::new(id);
    let t0 = Instant::now();
    let workers: Vec<_> = (0..conc)
        .map(|_| {
            let (node, next, id) = (node.clone(), next.clone(), id.clone());
            tokio::spawn(async move {
                let mut lat = Vec::with_capacity((n / conc + 1).min(1 << 20));
                let mut failed = 0;
                loop {
                    let i = next.fetch_add(1, Relaxed);
                    if i >= n || deadline.is_some_and(|d| Instant::now() >= d) {
                        return (lat, failed);
                    }
                    let t = Instant::now();
                    if once(&node, app, &id(i), method, path).await != 200 {
                        failed += 1;
                    }
                    lat.push(t.elapsed().as_micros() as u32);
                }
            })
        })
        .collect();
    let mut lat = Vec::new();
    let mut failed = 0;
    for w in workers {
        let (l, f) = w.await.unwrap();
        lat.extend(l);
        failed += f;
    }
    let elapsed = t0.elapsed();
    lat.sort_unstable();
    Run { elapsed, lat, failed }
}

fn row(name: &str, r: &Run, note: &str) {
    println!(
        "  {name:<34} {:>9.0}/s   p50 {:>7.2} ms   p99 {:>7.2} ms   p99.9 {:>7.2} ms   {}{note}",
        r.rps(),
        r.pct(0.5),
        r.pct(0.99),
        r.pct(0.999),
        if r.failed > 0 { format!("FAILED {}  ", r.failed) } else { String::new() },
    );
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let quick = args.iter().any(|a| a == "--quick");
    let json_out = args.iter().position(|a| a == "--json").and_then(|i| args.get(i + 1)).cloned();
    let scale = if quick { 10 } else { 1 };
    let cold_n = 10_000 / scale;
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
    let n = node("main", cold_n + 64)?;
    once(&n, "counter", "warmup", "GET", "/").await;
    let rss0 = rss_bytes();
    let cold = load(&n, "counter", "GET", "/", cold_n, conc, |i| format!("o{i}")).await;
    let live = n.live_objects();
    let rss1 = rss_bytes();
    let per_obj_kb = rss1.saturating_sub(rss0) as f64 / live.max(1) as f64 / 1024.0;
    let inst = n.metrics.instantiate.snapshot();
    println!("\ncold start ({cold_n} new objects, {conc} concurrent)");
    row("first request to a new object", &cold, "");
    println!(
        "  {:<34} {live} live · RSS {:.0} MB · {per_obj_kb:.0} KB/object · instantiate p50 ≤{} µs",
        "",
        rss1 as f64 / 1e6,
        inst.p50_us
    );
    out.insert("cold".into(), json!({ "run": cold.json(), "live": live, "rss_mb": rss1 as f64 / 1e6, "kb_per_object": per_obj_kb.round() }));

    let warm = load(&n, "counter", "GET", "/", warm_n, conc, move |i| format!("o{}", i % warm_objects)).await;
    let serial = load(&n, "counter", "GET", "/", 20_000 / scale, 1, |_| "o0".into()).await;
    let incr = load(&n, "counter", "POST", "/incr", warm_n / 5, conc, move |i| format!("o{}", i % warm_objects)).await;
    let incr1 = load(&n, "counter", "POST", "/incr", 2_000 / scale, 1, |_| "o0".into()).await;
    println!("\nwarm ({warm_objects} live objects)");
    row(&format!("GET, {conc} concurrent"), &warm, "");
    row("GET, one client, one object", &serial, "");
    row(&format!("POST /incr (writes /data), {conc} conc"), &incr, "");
    row("POST /incr, one client, one object", &incr1, "");
    out.insert("warm_get".into(), warm.json());
    out.insert("warm_serial".into(), serial.json());
    out.insert("warm_incr".into(), incr.json());
    out.insert("warm_incr_serial".into(), incr1.json());
    drop(n);

    // ---- kill and recover ----
    let n = node("kill", 256)?;
    let kills = 300 / scale;
    let mut lat = Vec::with_capacity(kills);
    let t0 = Instant::now();
    let mut failed = 0;
    for i in 0..kills {
        let id = format!("k{}", i % 16);
        once(&n, "hostile", &id, "GET", "/panic").await;
        let t = Instant::now();
        if once(&n, "hostile", &id, "GET", "/").await != 200 {
            failed += 1;
        }
        lat.push(t.elapsed().as_micros() as u32);
    }
    lat.sort_unstable();
    let recover = Run { elapsed: t0.elapsed(), lat, failed };
    println!("\nkill and recover ({kills} traps, serial)");
    row("next request after a trap", &recover, "(fresh instance)");
    out.insert("recover".into(), recover.json());
    drop(n);

    // ---- churn past capacity: every request evicts an idle object ----
    let cap = 1_000 / scale;
    let n = node("churn", cap)?;
    let churn = load(&n, "counter", "GET", "/", 20_000 / scale, 64, |i| format!("c{i}")).await;
    println!("\nchurn ({} new objects through {cap} slots, 64 concurrent)", 20_000 / scale);
    row("new object + LRU eviction", &churn, "");
    println!("  {:<34} {} evicted", "", n.metrics.evicted_lru.load(Relaxed));
    out.insert("churn".into(), churn.json());
    drop(n);

    // ---- neighbours under attack ----
    let n = node("attack", 1_000)?;
    let objs = 100 / scale.min(10);
    load(&n, "counter", "GET", "/", objs, 64, |i| format!("v{i}")).await;
    let window = Duration::from_millis(if quick { 1000 } else { 3000 });
    let base = load_until(&n, "counter", "GET", "/", usize::MAX, Some(Instant::now() + window), 64, move |i| {
        format!("v{}", i % objs)
    })
    .await;
    let stop = Arc::new(AtomicBool::new(false));
    let attackers: Vec<_> = (0..16)
        .map(|a| {
            let (n, stop) = (n.clone(), stop.clone());
            tokio::spawn(async move {
                let paths = ["/spin", "/alloc?mb=1024", "/panic", "/leak"];
                let mut k = 0;
                while !stop.load(Relaxed) {
                    once(&n, "hostile", &format!("a{a}"), "GET", paths[k % paths.len()]).await;
                    k += 1;
                }
                k
            })
        })
        .collect();
    // Let the attackers get going before measuring.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let killed0: u64 = n.metrics.poisons.iter().map(|p| p.load(Relaxed)).sum();
    let under = load_until(&n, "counter", "GET", "/", usize::MAX, Some(Instant::now() + window), 64, move |i| {
        format!("v{}", i % objs)
    })
    .await;
    let poisons = n.metrics.poisons.iter().map(|p| p.load(Relaxed)).sum::<u64>() - killed0;
    stop.store(true, Relaxed);
    let mut attacks = 0;
    for a in attackers {
        attacks += a.await?;
    }
    println!("\nneighbours ({objs} objects, 64 concurrent, {} s each)", window.as_secs_f64());
    row("baseline", &base, "");
    row("while 16 attackers loop", &under, &format!("({attacks} attacks, {poisons} instances killed)"));
    out.insert("neighbours".into(), json!({ "baseline": base.json(), "under_attack": under.json(), "attacks": attacks, "killed": poisons }));

    if let Some(path) = json_out {
        std::fs::write(&path, serde_json::to_string_pretty(&Value::Object(out))?)?;
        println!("\nwrote {path}");
    }
    Ok(())
}
