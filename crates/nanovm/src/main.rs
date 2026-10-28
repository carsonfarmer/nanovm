use std::path::PathBuf;

use anyhow::{Context, bail};
use nanovm::config::NodeConfig;
use nanovm::supervisor::Supervisor;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [cmd, flag, path] if cmd == "serve" && flag == "--config" => {
            let cfg = NodeConfig::load(&PathBuf::from(path))?;
            tokio::runtime::Runtime::new()?.block_on(serve(cfg))
        }
        // Started by the supervisor only.
        [cmd] if cmd == "worker" => nanovm::supervisor::worker_main(),
        _ => bail!("usage: nanovm serve --config <nanovm.toml>"),
    }
}

async fn serve(cfg: NodeConfig) -> anyhow::Result<()> {
    let listen = cfg.listen.clone();
    let t0 = std::time::Instant::now();
    let sup = Supervisor::new(cfg, std::env::current_exe()?).await?;
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    eprintln!(
        "nanovm: ready in {:.0?} on http://{listen}/ (stats: /_nanovm/stats)",
        t0.elapsed()
    );
    nanovm::server::serve_http1(listener, move |req| sup.clone().handle(req)).await
}
