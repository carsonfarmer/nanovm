use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use nanovm::config::NodeConfig;
use nanovm::supervisor::Supervisor;

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
        _ => bail!("usage: nanovm serve --config <nanovm.toml> | nanovm proxy --config <proxy.toml>"),
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
    // Its own pool of links (the one after the front threads'): a link it
    // opened, such as a readiness probe's, is driven by its reactor.
    let control = tokio::runtime::Builder::new_multi_thread().worker_threads(1)
        .thread_name("nanovm-control").on_thread_start(move || nanovm::link::set_thread(threads))
        .enable_all().build()?;
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
