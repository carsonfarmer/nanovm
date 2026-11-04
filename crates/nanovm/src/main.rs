use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use nanovm::config::{AppConfig, NodeConfig};
use nanovm::object::RespBody;
use nanovm::supervisor::Supervisor;
use nanovm::table::Node;
use nanovm::ws;

#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing_subscriber::filter::LevelFilter::WARN).with_writer(std::io::stderr).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [cmd, flag, path] if cmd == "serve" && flag == "--config" => serve(NodeConfig::load(&PathBuf::from(path))?),
        [cmd, flag, path] if cmd == "proxy" && flag == "--config" => {
            let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
            let cfg: nanovm::config::ProxyConfig = toml::from_str(&text).with_context(|| format!("parsing {path}"))?;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let listener = tokio::net::TcpListener::bind(&cfg.listen).await.with_context(|| format!("binding {}", cfg.listen))?;
                let publish = tokio::net::TcpListener::bind(&cfg.publish_listen).await?;
                let proxy = nanovm::proxy::Proxy::new(cfg)?;
                tokio::try_join!(nanovm::proxy::serve(proxy.clone(), listener), nanovm::proxy::serve_publish(proxy, publish))?;
                Ok(())
            })
        }
        // Started by the supervisor only, which reports the worker's last line.
        [cmd] if cmd == "worker" => nanovm::supervisor::worker_main().map_err(|e| {
            eprintln!("{e:#}");
            std::process::exit(1)
        }),
        [cmd, wasm, flags @ ..] if cmd == "run" => run(wasm, flags),
        _ => bail!("usage: nanovm serve --config <nanovm.toml> | nanovm proxy --config <proxy.toml> | {RUN}"),
    }
}

const RUN: &str = "nanovm run <app.wasm> [--manifest nanovm.json] [--listen 127.0.0.1:8080] [--data nanovm-data]";

/// `nanovm run`: one app in this process, for local development. With no
/// registry, signatures or jail, it listens on loopback only. It serves
/// `/{object}/…` as a node does, holds WebSockets as the proxy does, and
/// restarts when the component changes.
fn run(wasm: &str, flags: &[String]) -> anyhow::Result<()> {
    let (mut cfg, mut manifest) = (NodeConfig::default(), None);
    for flag in flags.chunks(2) {
        match flag {
            [k, v] if k == "--manifest" => manifest = Some(std::fs::read(v).with_context(|| format!("reading {v}"))?),
            [k, v] if k == "--listen" => cfg.listen = v.clone(),
            [k, v] if k == "--data" => cfg.data_dir = v.into(),
            _ => bail!("usage: {RUN}"),
        }
    }
    let addr: SocketAddr = cfg.listen.parse().with_context(|| format!("--listen {}", cfg.listen))?;
    anyhow::ensure!(addr.ip().is_loopback(), "nanovm run runs unsigned apps outside the jail, so only on loopback, not {addr}");
    let spec: nanovm::registry::AppSpec = serde_json::from_slice(manifest.as_deref().unwrap_or(b"{}")).context("the manifest")?;
    let name = Path::new(wasm).file_stem().and_then(|s| s.to_str()).filter(|s| nanovm::table::valid_name(s));
    let name = name.context("the component's file name must be <app>.wasm, with <app> of [A-Za-z0-9_.-]")?;
    let limits = spec.limits.clamped(&cfg.limits);
    let app = AppConfig { name: name.into(), digest: "run".into(), env: spec.env, config: spec.config, limits, worker: Default::default() };
    let bytes = std::fs::read(wasm).with_context(|| format!("reading {wasm}"))?;
    nanovm::engine::preflight(&cfg)?;
    tokio::runtime::Runtime::new()?.block_on(async {
        let console = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        cfg.telemetry_endpoint = format!("http://{}", console.local_addr()?);
        tokio::spawn(nanovm::server::serve_http1(console, print));
        let node = Node::new(cfg, &app, &bytes, None)?;
        let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
        eprintln!("nanovm: {name} on http://{}/<object>/ with data in {}", listener.local_addr()?, node.cfg.data_dir.display());
        tokio::spawn(restart_on_change(PathBuf::from(wasm)));
        nanovm::server::serve_http1(listener, move |req| local(node.clone(), req)).await
    })
}

/// What the proxy adds in front of a node: it holds WebSockets, and only it
/// sends their events.
async fn local(node: Arc<Node>, req: http::Request<hyper::body::Incoming>) -> http::Response<RespBody> {
    if ws::is_event(&req) {
        return nanovm::object::error_response(400, "proxy", "socket events come from the proxy");
    }
    let object = nanovm::table::route(req.uri().path()).map(|(id, _)| id.to_string());
    let Some(object) = object.filter(|_| ws::wants(&req)) else { return nanovm::server::handle(node, req).await };
    let (n, max) = (node.clone(), (nanovm::config::ProxyConfig::default().max_response_body_kb << 10) as usize);
    let deliver: ws::Deliver = Box::new(move |req| Box::pin(event(n.clone(), req, max)));
    node.sockets.clone().upgrade(object, req.map(http_body_util::BodyExt::boxed_unsync), deliver).await
}

async fn event<B: http_body::Body<Data = bytes::Bytes, Error = hyper::Error>>(node: Arc<Node>, req: http::Request<B>, max: usize) -> ws::Reply {
    use http_body_util::BodyExt;
    // Reboxed with a concrete error: awaiting a `dyn Error` one trips rustc's `Send` check.
    let res = nanovm::server::handle(node, req).await.map(|b| b.map_err(|e| std::io::Error::other(e.to_string())).boxed_unsync());
    ws::answer(res, max).await
}

/// Prints the telemetry `serve` exports to a Collector: a line for each
/// request and runtime event, and what guests write to stdout and stderr.
async fn print(req: http::Request<hyper::body::Incoming>) -> http::Response<RespBody> {
    use http_body_util::BodyExt;
    use opentelemetry_proto::tonic::collector::{logs::v1::ExportLogsServiceRequest as Logs, trace::v1::ExportTraceServiceRequest as Traces};
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value::Value};
    use prost::Message;
    let show = |v: Option<&AnyValue>| match v.and_then(|v| v.value.as_ref()) {
        Some(Value::StringValue(s)) => s.trim().to_string(),
        Some(Value::IntValue(i)) => i.to_string(),
        Some(Value::BoolValue(b)) => b.to_string(),
        _ => String::new(),
    };
    let get = |a: &[KeyValue], key: &str| show(a.iter().find(|a| a.key == key).and_then(|a| a.value.as_ref()));
    let traces = req.uri().path() == "/v1/traces";
    let body = req.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
    if traces {
        let spans = Traces::decode(body).into_iter().flat_map(|t| t.resource_spans).flat_map(|r| r.scope_spans);
        for s in spans.flat_map(|s| s.spans).filter(|s| s.name == "request") {
            let a = &s.attributes;
            let ms = s.end_time_unix_nano.saturating_sub(s.start_time_unix_nano) as f64 / 1e6;
            let cold = if get(a, "nanovm.cold_start") == "true" { ", cold start" } else { "" };
            let (object, method, path) = (get(a, "nanovm.object"), get(a, "http.request.method"), get(a, "url.path"));
            let status = [get(a, "http.response.status_code"), get(a, "error.type")].join(" ");
            eprintln!("{method} /{object}{path} → {} in {ms:.1} ms{cold}", status.trim());
        }
    } else {
        let logs = Logs::decode(body).into_iter().flat_map(|l| l.resource_logs).flat_map(|r| r.scope_logs);
        for l in logs.flat_map(|s| s.log_records) {
            let kind = [get(&l.attributes, "event.name"), get(&l.attributes, "log.iostream")].concat();
            if kind != "nanovm.snapshot" {
                eprintln!("/{} {kind}: {}", get(&l.attributes, "nanovm.object"), show(l.body.as_ref()));
            }
        }
    }
    http::Response::new(http_body_util::Empty::new().map_err(|e| match e {}).boxed_unsync())
}

/// Runs this command afresh once `wasm` has changed and settled. Requests in
/// flight end as in a crash, so their changes roll back; sockets close.
async fn restart_on_change(wasm: PathBuf) {
    use std::os::unix::process::CommandExt;
    let stamp = || std::fs::metadata(&wasm).and_then(|m| m.modified()).ok();
    let (first, mut last) = (stamp(), stamp());
    loop {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let now = stamp();
        if now.is_some() && now != first && now == last {
            eprintln!("nanovm: {} changed; restarting", wasm.display());
            let exe = std::env::current_exe().unwrap_or_else(|_| "nanovm".into());
            let e = std::process::Command::new(exe).args(std::env::args_os().skip(1)).exec();
            eprintln!("nanovm: restarting: {e}");
            return;
        }
        last = now;
    }
}

/// Each front-end thread runs its own single-threaded runtime and accepts on
/// its own `SO_REUSEPORT` listener, so a request is handled on one thread
/// from accept to response, through that thread's connections to the worker.
fn serve(cfg: NodeConfig) -> anyhow::Result<()> {
    let addr: SocketAddr = cfg.listen.parse().with_context(|| format!("listen = {:?}", cfg.listen))?;
    let threads = cfg.front_threads();
    let t0 = std::time::Instant::now();
    let rt = runtime()?;
    let control = tokio::runtime::Builder::new_multi_thread().worker_threads(1)
        .thread_name("nanovm-control").enable_all().build()?;
    let sup = control.block_on(async { Supervisor::new(cfg, std::env::current_exe()?, threads) })?;
    let first = rt.block_on(async { listener(addr) })?;
    eprintln!("nanovm: ready in {:.0?} on http://{addr}/", t0.elapsed());
    for i in 1..threads {
        let sup = sup.clone();
        std::thread::Builder::new()
            .name(format!("nanovm-front-{i}"))
            .spawn(move || -> anyhow::Result<()> {
                nanovm::link::set_thread(i);
                let rt = runtime()?;
                rt.block_on(async { front(sup, listener(addr)?).await })
            })?;
    }
    // Control/egress I/O has its own reactor, independent of front-end load.
    rt.block_on(front(sup, first))
}

fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()
}

fn listener(addr: SocketAddr) -> anyhow::Result<tokio::net::TcpListener> {
    let sock = if addr.is_ipv4() {
        tokio::net::TcpSocket::new_v4()?
    } else {
        tokio::net::TcpSocket::new_v6()?
    };
    sock.set_reuseaddr(true)?;
    sock.set_reuseport(true)?;
    sock.bind(addr).with_context(|| format!("binding {addr}"))?;
    Ok(sock.listen(1024)?)
}

async fn front(sup: Arc<Supervisor>, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    nanovm::server::serve_http1(listener, move |req| sup.clone().handle(req)).await
}
