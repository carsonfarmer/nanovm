//! One command for the whole routed path on loopback: an OCI registry, a
//! stub DNS-over-HTTPS resolver serving `_nanovm` TXT records, a dev-mode
//! node (`nanovm serve`, with its jailed workers) and the proxy, each app
//! deployed under a `*.localhost` name. Rebuilding an app's component
//! redeploys it: the next request runs the new version.
//!
//!     tools/local-stack [--listen 127.0.0.1:8443] [--registry 127.0.0.1:5050]
//!                       [--dir target/local-stack] [--fresh] [HOST=APP.wasm[,nanovm.json] ...]
//!
//! Without apps it deploys the test guests (scripts/build-guests.sh) as
//! counter.localhost and hostile.localhost. The registry and the node's
//! volumes persist in --dir across runs (--fresh wipes it); an app's volumes
//! belong to its host and repository, registry address included.

#[path = "../support/stack.rs"]
mod stack;

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, bail};

struct App {
    host: String,
    wasm: PathBuf,
    spec: Option<PathBuf>,
}

impl App {
    fn read(&self) -> anyhow::Result<(Vec<u8>, Vec<u8>, SystemTime)> {
        let wasm = std::fs::read(&self.wasm).with_context(|| format!("{}", self.wasm.display()))?;
        let modified = std::fs::metadata(&self.wasm)?.modified()?;
        let spec = match &self.spec {
            Some(p) => std::fs::read(p).with_context(|| format!("{}", p.display()))?,
            None => b"{}".to_vec(),
        };
        Ok((wasm, spec, modified))
    }

    fn repo(&self) -> String {
        self.host.split('.').next().unwrap_or("app").to_string()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let usage = "usage: local-stack [--nanovm PATH] [--listen ADDR] [--registry ADDR] [--dir DIR] [--fresh] \
                 [HOST=APP.wasm[,nanovm.json] ...]";
    let (mut nanovm, mut listen, mut dir, mut fresh, mut apps) = (
        None,
        "127.0.0.1:8443".to_string(),
        root.join("target/local-stack"),
        false,
        vec![],
    );
    let mut registry = "127.0.0.1:5050".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--nanovm" => nanovm = Some(PathBuf::from(args.next().context(usage)?)),
            "--listen" => listen = args.next().context(usage)?,
            "--registry" => registry = args.next().context(usage)?,
            "--dir" => dir = PathBuf::from(args.next().context(usage)?),
            "--fresh" => fresh = true,
            _ => match a.split_once('=') {
                Some((host, files)) => {
                    let (wasm, spec) = match files.split_once(',') {
                        Some((w, s)) => (w.into(), Some(s.into())),
                        None => (files.into(), None),
                    };
                    apps.push(App {
                        host: host.to_ascii_lowercase(),
                        wasm,
                        spec,
                    });
                }
                None => bail!("{usage}"),
            },
        }
    }
    if apps.is_empty() {
        for guest in ["counter", "hostile"] {
            apps.push(App {
                host: format!("{guest}.localhost"),
                wasm: root.join(format!("guests/target/wasm32-wasip2/release/{guest}.wasm")),
                spec: Some(root.join(format!("guests/{guest}/nanovm.json"))),
            });
        }
    }
    // Next to this example by default: target/<profile>/nanovm.
    let nanovm = match nanovm {
        Some(p) => p,
        None => std::env::current_exe()?
            .parent()
            .and_then(|p| p.parent())
            .context("no target dir")?
            .join("nanovm"),
    };
    if fresh {
        let _ = std::fs::remove_dir_all(&dir);
    }
    let mut versions: Vec<(SystemTime, u64)> = vec![];
    let mut first = vec![];
    for app in &apps {
        first.push(app.read()?);
    }
    let stack = stack::Stack::start(stack::Options {
        nanovm: nanovm.clone(),
        dir: dir.clone(),
        listen: listen.parse().with_context(|| format!("--listen {listen}"))?,
        tag_ttl_s: 1,
        registry: registry.parse().with_context(|| format!("--registry {registry}"))?,
    })
    .await
    .with_context(|| {
        format!(
            "starting the stack ({} must be built with --features dev)",
            nanovm.display()
        )
    })?;
    let port = stack.proxy.port();
    println!("nanovm on loopback (configs and logs in {}):", stack.dir().display());
    println!("  proxy     http://{}  the front door: routes by Host", stack.proxy);
    println!("  node      http://{}  dev mode: apps unsigned", stack.node);
    println!("  registry  {}", stack.registry);
    println!("  resolver  {}", stack.resolver);
    println!("apps:");
    for (app, (wasm, spec, modified)) in apps.iter().zip(first) {
        let r = stack.deploy(&app.host, &app.repo(), wasm, spec, "1.0.0").await?;
        println!("  {} -> {r}", app.host);
        versions.push((modified, 0));
    }
    for (name, txt) in stack.records() {
        println!("  TXT {name} \"{}\"", txt.join("\" \""));
    }
    let host = &apps[0].host;
    println!("try:");
    println!("  curl http://{host}:{port}/alice/");
    println!("  curl -X POST http://{host}:{port}/alice/incr");
    println!("  (curl sends *.localhost to 127.0.0.1; or use -H 'Host: {host}' http://127.0.0.1:{port}/alice/)");
    println!("Rebuilding an app's .wasm redeploys it. Ctrl-C stops everything.");

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = term.recv() => break,
            _ = tick.tick() => {}
        }
        for (app, (seen, n)) in apps.iter().zip(versions.iter_mut()) {
            let Ok(modified) = std::fs::metadata(&app.wasm).and_then(|m| m.modified()) else {
                continue;
            };
            if modified == *seen {
                continue;
            }
            // Let a writer finish.
            tokio::time::sleep(Duration::from_millis(300)).await;
            match app.read() {
                Ok((wasm, spec, modified)) => {
                    *n += 1;
                    *seen = modified;
                    match stack
                        .deploy(&app.host, &app.repo(), wasm, spec, &format!("1.0.{n}"))
                        .await
                    {
                        Ok(r) => println!("redeployed {} as 1.0.{n}: {r}", app.host),
                        Err(e) => println!("redeploying {} failed: {e:#}", app.host),
                    }
                }
                Err(e) => println!("reading {}: {e:#}", app.wasm.display()),
            }
        }
    }
    stack.stop().await;
    Ok(())
}
