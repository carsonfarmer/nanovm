use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use nanovm::config::NodeConfig;
use nanovm::supervisor::Supervisor;

fn main() -> anyhow::Result<()> {
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
    let sup = Supervisor::new(cfg, std::env::current_exe()?, threads)?;
    let first = rt.block_on(async { listener(addr) })?;
    eprintln!("nanovm: ready in {:.0?} on http://{addr}/ (stats: /_nanovm/stats)", t0.elapsed());
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
    // Thread 0 also runs the workers' reapers and egress links.
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
