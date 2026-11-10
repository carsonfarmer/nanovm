//! Per-operation cost of the components in `components/`, through their test
//! apps on a real in-process node (no sockets). For each case: wall time and
//! process CPU (user+sys, from getrusage) per request, serially, and the
//! object's logical bytes on disk afterwards. Build the fixtures first.
//!
//!     cargo run --locked -p nanovm --example components_bench -- [filter] [--n 1000]
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, ensure};
use nanovm::config::{AppConfig, NodeConfig};
use nanovm::table::Node;

#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;

/// One measured case: `n` requests of `method path`, numbered `{i}` in the
/// path; `setup` requests run first, unmeasured.
struct Case {
    component: &'static str,
    name: &'static str,
    setup: &'static [(&'static str, &'static str, &'static str)],
    method: &'static str,
    path: &'static str,
    body: &'static str,
    /// Operations each request performs, to report per-op figures.
    ops: u64,
    /// Restart the node before each measured request: a cold instance that
    /// must load its state first.
    cold: bool,
}

const fn case(
    component: &'static str,
    name: &'static str,
    setup: &'static [(&'static str, &'static str, &'static str)],
    (method, path, body): (&'static str, &'static str, &'static str),
    ops: u64,
    cold: bool,
) -> Case {
    Case {
        component,
        name,
        setup,
        method,
        path,
        body,
        ops,
        cold,
    }
}

const NONE: (&str, &str, &str) = ("GET", "/none", "");
const DOCS_10K: &[(&str, &str, &str)] = &[
    ("POST", "/index?field=tag", ""),
    ("POST", "/bulk?count=10000&size=100", ""),
];
const QUEUE_1K: &[(&str, &str, &str)] = &[("POST", "/bulk?count=1000&size=100&batch=100", "")];

#[rustfmt::skip]
const CASES: &[Case] = &[
    // The composed KV provider's own test app, for reference.
    case("keyvalue", "reference: KV set 9 B / request", &[], ("GET", "/set", ""), 1, false),
    case("keyvalue", "reference: KV get / request", &[("GET", "/set", "")], ("GET", "/get", ""), 1, false),
    case("queue", "baseline: unknown route (404)", &[], NONE, 1, false),
    case("queue", "append 1 x 100 B / request", &[], ("POST", "/append", B100), 1, false),
    case("queue", "read 1 / request", QUEUE_1K, ("GET", "/read?offset={i}&max=1", ""), 1, false),
    case("queue", "next 1 (consume + commit position) / request", QUEUE_1K, ("POST", "/next?max=1", ""), 1, false),
    case("queue", "append batch of 100 x 100 B / request (per message)", &[], ("POST", "/bulk?count=100&size=100&batch=100", ""), 100, false),
    case("queue", "append 100 B x 1000 in one request (per append)", &[], ("POST", "/bulk?count=1000&size=100&batch=1", ""), 1000, false),
    case("docs", "baseline: unknown route (404)", &[], NONE, 1, false),
    case("docs", "put 1 doc (~150 B, 1 index) / request", &[("POST", "/index?field=tag", "")], ("PUT", "/doc?id=k{i}", DOC), 1, false),
    case("docs", "get 1 doc / request (of 10,000)", DOCS_10K, ("GET", "/doc?id=d00{i}", ""), 1, false),
    case("docs", "query tag = t3, 10 docs / request (of 10,000)", DOCS_10K, ("GET", "/query?field=tag&eq=%22t3%22&limit=10", ""), 1, false),
    case("docs", "put 1,000 docs in one request (per put)", &[("POST", "/index?field=tag", "")], ("POST", "/bulk?count=1000&size=100&prefix=b{i}-", ""), 1000, false),
    case("docs", "cold start: first get after restart, 10,000 docs, 1 index", DOCS_10K, ("GET", "/doc?id=d000001", ""), 1, true),
    case("docs", "cold start: first get after restart, 10,000 docs, no index", &[("POST", "/bulk?count=10000&size=100", "")], ("GET", "/doc?id=d000001", ""), 1, true),
    case("docs", "cold start: unknown route after restart (no load)", &[("POST", "/bulk?count=10000&size=100", "")], NONE, 1, true),
    case("timeseries", "baseline: unknown route (404)", &[], NONE, 1, false),
    case("timeseries", "write 1 point / request", &[], ("POST", "/write", "1000 1.5"), 1, false),
    case("timeseries", "write 1,000 points in one call (per point)", &[], ("POST", "/write?n=1000&start=0&every=1000", ""), 1000, false),
    case("timeseries", "aggregate 3 days by day (rollups) / request", TS_3D, ("GET", "/agg?from=0&to=259200000&step=86400000", ""), 1, false),
    case("timeseries", "aggregate 1 day by hour (rollups) / request", TS_3D, ("GET", "/agg?from=0&to=86400000&step=3600000", ""), 1, false),
    case("timeseries", "aggregate 1 day by 15 min (1,440 raw points) / request", TS_3D, ("GET", "/agg?from=0&to=86400000&step=900000", ""), 1, false),
    case("timeseries", "range 1 hour (60 raw points) / request", TS_3D, ("GET", "/range?from=0&to=3600000", ""), 1, false),
    case("templates", "baseline: unknown route (404)", &[], NONE, 1, false),
    case("templates", "render a 20-row page via the component / request", &[], ("GET", "/page?via=component", ""), 1, false),
    case("templates", "render a 20-row page via the linked crate / request", &[], ("GET", "/page?via=crate", ""), 1, false),
    case("templates", "100 renders via the component (per render)", &[], ("GET", "/page?via=component&n=100", ""), 100, false),
    case("templates", "100 renders via the linked crate (per render)", &[], ("GET", "/page?via=crate&n=100", ""), 100, false),
];

/// Three days of one point a minute.
const TS_3D: &[(&str, &str, &str)] = &[("POST", "/write?n=4320&start=0&every=60000", "")];

const DOC: &str = r#"{"tag":"t1","n":12345,"name":"Ada Lovelace","email":"ada@example.com","pad":"0123456789012345678901234567890123456789012345678901234567890123456789"}"#;
const B100: &str =
    "0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789";

fn cpu() -> f64 {
    let mut u = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    unsafe { libc::getrusage(libc::RUSAGE_SELF, u.as_mut_ptr()) };
    let u = unsafe { u.assume_init() };
    let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    t(u.ru_utime) + t(u.ru_stime)
}

fn logical_bytes(dir: &Path) -> (u64, u64) {
    let (mut bytes, mut files) = (0, 0);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let md = e.metadata().unwrap();
            files += 1;
            if md.is_dir() {
                stack.push(e.path());
            } else {
                bytes += md.len();
            }
        }
    }
    (bytes, files)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let n: usize = args
        .iter()
        .position(|a| a == "--n")
        .map_or(1000, |i| args[i + 1].parse().unwrap());
    let filter = args
        .iter()
        .find(|a| !a.starts_with("--") && a.parse::<usize>().is_err())
        .cloned();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir: PathBuf = root.join(format!("target/components-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let cfg = NodeConfig {
        data_dir: dir.clone(),
        max_live_objects: 64,
        memory_reservation_mb: 64,
        ..NodeConfig::default()
    };
    cfg.validate()?;
    nanovm::engine::preflight(&cfg)?;
    println!("| component | case | requests | wall µs/req | CPU µs/req | CPU µs/op | bytes/op on disk |");
    println!("|---|---|---:|---:|---:|---:|---:|");
    for (k, case) in CASES.iter().enumerate() {
        if filter
            .as_deref()
            .is_some_and(|f| !case.component.contains(f) && !case.name.contains(f))
        {
            continue;
        }
        let fixture = match case.component {
            "keyvalue" => root.join("target/keyvalue/tests.wasm"),
            c => root.join("target/components").join(c).join("tests.wasm"),
        };
        let wasm = std::fs::read(fixture).with_context(|| format!("build components/{}/build.sh", case.component))?;
        let app = AppConfig {
            name: case.component.into(),
            digest: "bench".into(),
            env: Default::default(),
            config: Default::default(),
            worker: Default::default(),
            limits: serde_json::from_value(serde_json::json!({
                "memory_mb": 128, "cpu_ms": 60000, "request_timeout_ms": 120000, "disk_mb": 1024, "max_files": 100000
            }))?,
        };
        let mut node = Node::new(cfg.clone(), &app, &wasm, None)?;
        let id = format!("case{k}");
        for (method, path, body) in case.setup {
            let r = load::request(&node, method, &id, path, body).await;
            ensure!(r.status == 200, "setup {path}: {} {}", r.status, r.body);
        }
        // Warm the instance (and the compile cache) before measuring.
        let _ = load::request(&node, "GET", &id, "/none", "").await;
        let volume = dir.join("objects").join(case.component).join(&id);
        let (bytes0, _) = logical_bytes(&volume);
        let reqs = match (case.cold, case.ops) {
            (true, _) => 10,
            (false, 1) => n,
            (false, ops) => (n * 10 / ops as usize).max(3),
        };
        let (mut wall, mut used) = (0.0, 0.0);
        for i in 0..reqs {
            if case.cold {
                node.shutdown().await;
                drop(node);
                node = Node::new(cfg.clone(), &app, &wasm, None)?;
            }
            let path = case.path.replace("{i}", &format!("{i:04}"));
            let (t0, c0) = (Instant::now(), cpu());
            let r = load::request(&node, case.method, &id, &path, case.body).await;
            (wall, used) = (wall + t0.elapsed().as_secs_f64(), used + cpu() - c0);
            ensure!(r.status == 200 || path == "/none", "{path}: {} {}", r.status, r.body);
        }
        let (bytes1, _) = logical_bytes(&volume);
        let per_req = used / reqs as f64 * 1e6;
        println!(
            "| {} | {} | {} | {:.0} | {:.0} | {:.1} | {} |",
            case.component,
            case.name,
            reqs,
            wall / reqs as f64 * 1e6,
            per_req,
            per_req / case.ops as f64,
            match bytes1.checked_sub(bytes0) {
                Some(0) | None => "-".to_string(),
                Some(d) => format!("{:.1}", d as f64 / (reqs as u64 * case.ops) as f64),
            }
        );
        node.shutdown().await;
    }
    std::fs::remove_dir_all(dir)?;
    Ok(())
}
