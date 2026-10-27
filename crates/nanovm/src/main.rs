use std::path::PathBuf;

use anyhow::{Context, bail};
use nanovm::config::NodeConfig;
use nanovm::table::Node;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = match args.as_slice() {
        [cmd, flag, path] if cmd == "serve" && flag == "--config" => PathBuf::from(path),
        _ => bail!("usage: nanovm serve --config <nanovm.toml>"),
    };
    let cfg = NodeConfig::load(&config)?;
    let listen = cfg.listen.clone();
    let t0 = std::time::Instant::now();
    let node = Node::new(cfg)?;
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let mut apps: Vec<_> = node.apps.keys().cloned().collect();
    apps.sort();
    eprintln!(
        "nanovm: {} app(s) [{}] ready in {:.0?}, listening on http://{listen}/ (dashboard: http://{listen}/_nanovm/)",
        apps.len(),
        apps.join(", "),
        t0.elapsed()
    );
    nanovm::server::serve(node, listener).await
}
