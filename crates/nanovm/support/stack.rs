//! A whole nanovm on loopback: an OCI registry, a DNS-over-HTTPS stub that
//! answers `_nanovm` TXT records, a dev-mode node (`nanovm serve`: the
//! supervisor and its jailed workers) and the proxy (`nanovm proxy`) in
//! front of it, each process with its own config file. Requests to the
//! proxy take the full routed path: DNS record, tag, digest, node, worker.
//! Used by `tools/local-stack` (examples/local-stack.rs) and tests/stack.rs.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use nanovm::registry::{APP_TYPE, CONFIG_TYPE, Oci, VERSION, WASM_TYPE};
use oci_client::client::{ClientConfig, ClientProtocol, Config, ImageLayer};
use oci_client::manifest::{OCI_IMAGE_MEDIA_TYPE, OciImageManifest};

/// `_nanovm.<host>` → its TXT records.
type Records = Arc<parking_lot::Mutex<HashMap<String, Vec<String>>>>;

pub struct Options {
    /// A `nanovm` binary built with `--features dev`.
    pub nanovm: PathBuf,
    /// Configs, logs, the registry's storage and the node's data_dir.
    pub dir: PathBuf,
    /// Where the proxy listens (port 0: any).
    pub listen: SocketAddr,
    /// How long the proxy keeps a tag's digest; 0 asks every time.
    pub tag_ttl_s: u64,
    /// Where the registry listens. An app's identity (so its volumes)
    /// includes the registry's address: keep it to keep the data.
    pub registry: SocketAddr,
}

pub struct Stack {
    pub proxy: SocketAddr,
    pub node: SocketAddr,
    pub registry: SocketAddr,
    pub resolver: String,
    records: Records,
    children: Vec<tokio::process::Child>,
    dir: PathBuf,
    _registry: container_registry::test_support::RunningRegistry,
}

/// A loopback address with a free port, for a child process to bind.
fn free_port() -> anyhow::Result<SocketAddr> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?)
}

impl Stack {
    pub async fn start(o: Options) -> anyhow::Result<Stack> {
        std::fs::create_dir_all(&o.dir)?;
        let dir = o.dir.canonicalize()?;
        // As root, each app's worker runs as its own uid and must reach its
        // directories (tools/doctor checks the same).
        if unsafe { libc::geteuid() } == 0 {
            use std::os::unix::fs::PermissionsExt;
            for d in dir.ancestors() {
                if std::fs::metadata(d)?.permissions().mode() & 0o001 == 0 {
                    bail!(
                        "{} is not searchable by others, so app workers cannot reach {}: use another --dir",
                        d.display(),
                        dir.display()
                    );
                }
            }
        }
        std::fs::create_dir_all(dir.join("registry"))?;
        let mut registry = container_registry::ContainerRegistry::builder()
            .storage(dir.join("registry"))
            .build_for_testing();
        std::net::TcpListener::bind(o.registry).with_context(|| format!("the registry's address {}", o.registry))?;
        registry.bind(o.registry);
        let registry = registry.run_in_background();
        let records: Records = Default::default();
        let resolver = doh(records.clone()).await?;
        let telemetry = sink().await?;

        let (node, publish) = (free_port()?, free_port()?);
        let node_cfg = nanovm::config::NodeConfig {
            listen: node.to_string(),
            data_dir: dir.join("data"),
            publish_to: format!("http://{publish}/publish"),
            telemetry_endpoint: telemetry.clone(),
            max_live_objects: 64,
            dev: true,
            ..Default::default()
        };
        std::fs::write(dir.join("nanovm.toml"), toml::to_string(&node_cfg)?)?;
        let listener = std::net::TcpListener::bind(o.listen).with_context(|| format!("binding {}", o.listen))?;
        let proxy = listener.local_addr()?;
        drop(listener);
        let proxy_cfg = nanovm::config::ProxyConfig {
            listen: proxy.to_string(),
            node: node.to_string(),
            publish_listen: publish.to_string(),
            resolver: resolver.clone(),
            tag_ttl_s: o.tag_ttl_s,
            telemetry_endpoint: telemetry,
            dev: true,
            ..Default::default()
        };
        std::fs::write(dir.join("proxy.toml"), toml::to_string(&proxy_cfg)?)?;

        let mut stack = Stack {
            proxy,
            node,
            registry: registry.bound_addr(),
            resolver,
            records,
            children: vec![],
            dir: dir.clone(),
            _registry: registry,
        };
        for (what, cmd) in [("node", "serve"), ("proxy", "proxy")] {
            let config = dir.join(if what == "node" { "nanovm.toml" } else { "proxy.toml" });
            let child = tokio::process::Command::new(&o.nanovm)
                .args([cmd, "--config"])
                .arg(&config)
                .env_remove("RUST_BACKTRACE")
                .stderr(std::fs::File::create(dir.join(format!("{what}.log")))?)
                .kill_on_drop(true)
                .spawn()
                .with_context(|| format!("starting {}", o.nanovm.display()))?;
            stack.children.push(child);
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(1))
            .build()?;
        let t0 = Instant::now();
        loop {
            for (i, c) in stack.children.iter_mut().enumerate() {
                if let Some(status) = c.try_wait()? {
                    let log = std::fs::read_to_string(dir.join(["node.log", "proxy.log"][i])).unwrap_or_default();
                    bail!("the {} exited ({status}):\n{log}", ["node", "proxy"][i]);
                }
            }
            let ready = client
                .get(format!("http://{node}/_nanovm/ready"))
                .send()
                .await
                .is_ok_and(|r| r.status() == 204);
            // The proxy answers a host without a record itself.
            let routed = client
                .get(format!("http://{proxy}/"))
                .header("host", "nowhere.localhost")
                .send()
                .await
                .is_ok();
            if ready && routed {
                return Ok(stack);
            }
            if t0.elapsed() > Duration::from_secs(30) {
                bail!("the stack did not come up in 30 s; see {}", dir.display());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Pushes `wasm` with its `nanovm.json` (`spec`) to the registry as
    /// `nanovm/<repo>:latest` at `version`, and points `host` at that tag: a
    /// deploy. Returns the pushed reference by digest.
    pub async fn deploy(
        &self,
        host: &str,
        repo: &str,
        wasm: Vec<u8>,
        spec: Vec<u8>,
        version: &str,
    ) -> anyhow::Result<String> {
        let client = oci_client::Client::new(ClientConfig {
            protocol: ClientProtocol::Http,
            ..Default::default()
        });
        let (layer, config) = (
            ImageLayer::new(wasm, WASM_TYPE.into(), None),
            Config::new(spec, CONFIG_TYPE.into(), None),
        );
        let annotations = [(VERSION.to_string(), version.to_string())].into();
        let mut manifest = OciImageManifest::build(std::slice::from_ref(&layer), &config, Some(annotations));
        (manifest.media_type, manifest.artifact_type) = (Some(OCI_IMAGE_MEDIA_TYPE.into()), Some(APP_TYPE.into()));
        let tag: oci_client::Reference = format!("{}/nanovm/{repo}:latest", self.registry).parse()?;
        let auth = oci_client::secrets::RegistryAuth::Anonymous;
        client.push(&tag, &[layer], config, &auth, Some(manifest)).await?;
        let digest = Oci::new(true)?.digest(&tag).await?;
        let txt = format!("app={}/nanovm/{repo}:latest; trust=dev dev", self.registry);
        self.records
            .lock()
            .insert(format!("_nanovm.{}", host.to_ascii_lowercase()), vec![txt]);
        Ok(format!("{}/nanovm/{repo}@{digest}", self.registry))
    }

    /// The TXT records the stub serves, by name.
    pub fn records(&self) -> Vec<(String, Vec<String>)> {
        let mut r: Vec<_> = self
            .records
            .lock()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        r.sort();
        r
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Stops the node and the proxy, and waits for them.
    pub async fn stop(mut self) {
        for c in &mut self.children {
            let _ = c.kill().await;
        }
    }
}

/// A DNS-over-HTTPS stub (the JSON API the proxy asks) answering TXT
/// queries from `records`; a record `CNAME <name>` answers as an alias.
async fn doh(records: Records) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(nanovm::server::serve_http1(
        listener,
        move |req: http::Request<hyper::body::Incoming>| {
            let q = req.uri().query().unwrap_or("");
            let mut name = q
                .split('&')
                .find_map(|kv| kv.strip_prefix("name="))
                .unwrap_or("")
                .to_ascii_lowercase();
            let mut answer = Vec::new();
            let target = records
                .lock()
                .get(&name)
                .and_then(|r| r.first()?.strip_prefix("CNAME ").map(String::from));
            if let Some(target) = target {
                answer.push(serde_json::json!({ "name": name, "type": 5, "TTL": 1, "data": format!("{target}.") }));
                name = target;
            }
            answer.extend(records.lock().get(&name).into_iter().flatten().map(|txt| {
            serde_json::json!({ "name": format!("{name}."), "type": 16, "TTL": 1, "data": format!("\"{txt}\"") })
        }));
            async move { nanovm::server::ok_json(serde_json::json!({ "Status": 0, "Answer": answer })) }
        },
    ));
    Ok(format!("http://{addr}/dns-query"))
}

/// An OTLP/HTTP endpoint that accepts and drops everything, so the stack's
/// telemetry goes nowhere rather than to whatever listens on 4318.
async fn sink() -> anyhow::Result<String> {
    use http_body_util::BodyExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(nanovm::server::serve_http1(
        listener,
        |req: http::Request<hyper::body::Incoming>| async move {
            let _ = req.into_body().collect().await;
            http::Response::new(
                http_body_util::Full::new(bytes::Bytes::new())
                    .map_err(|e| match e {})
                    .boxed_unsync(),
            )
        },
    ));
    Ok(format!("http://{addr}"))
}
