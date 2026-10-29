//! An app is one component plus its limits. Loading it compiles (or reuses a
//! cached compile), checks the component's imports against the capabilities
//! the node offers, and pre-links it so instantiation is cheap.

use std::sync::Arc;

use anyhow::{Context, bail};
use wasmtime::Engine;
use wasmtime::component::{Component, Linker};
use wasmtime_wasi::filesystem::Dir;
use wasmtime_wasi::{FsPerms, OpenMode};
use wasmtime_wasi_http::p3::bindings::ServicePre;

use crate::config::{AppConfig, NodeConfig};
use crate::egress::{EgressPolicy, Route, Upstream};
use crate::object::ObjState;

/// Import prefixes a guest may use. Everything else (sockets, the WASIp2
/// filesystem, anything custom) is refused at load time, so the only paths
/// out of the sandbox are the quota'd filesystem and allowlisted HTTP.
const ALLOWED_IMPORTS: &[&str] = &[
    "wasi:io/",
    "wasi:clocks/",
    "wasi:random/",
    "wasi:cli/",
    "wasi:filesystem/types@0.3",
    "wasi:filesystem/preopens@0.3",
    "wasi:http/types@0.3",
    "wasi:http/client@0.3",
    "wasi:keyvalue/store@0.2.0-draft",
    "wasi:keyvalue/atomics@0.2.0-draft",
    "wasi:keyvalue/batch@0.2.0-draft",
];

pub struct App {
    pub name: Arc<str>,
    pub cfg: AppConfig,
    pub pre: ServicePre<ObjState>,
    pub egress: Route,
    /// `/app`, opened once and shared read-only by every object of the app,
    /// so it costs one fd per app rather than one per live object.
    pub static_dir: Option<Dir>,
}

pub fn linker(engine: &Engine) -> anyhow::Result<Linker<ObjState>> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    crate::quota::add_to_linker(&mut linker)?;
    crate::kv::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    Ok(linker)
}

/// Rejects components that import anything outside [`ALLOWED_IMPORTS`].
pub fn check_imports(engine: &Engine, component: &Component) -> anyhow::Result<()> {
    let ty = component.component_type();
    for (name, _) in ty.imports(engine) {
        if !ALLOWED_IMPORTS.iter().any(|p| name.starts_with(p)) {
            bail!("import `{name}` is not permitted");
        }
    }
    // A missing `wasi:http/handler` export fails `ServicePre::new`, by name.
    Ok(())
}

impl App {
    pub fn load(
        engine: &Engine,
        linker: &Linker<ObjState>,
        node: &NodeConfig,
        cfg: &AppConfig,
        wasm: &[u8],
        upstream: Option<Upstream>,
    ) -> anyhow::Result<App> {
        let [_, _, cache] = node.app_dirs(&cfg.name);
        let component = crate::engine::load_component(engine, wasm, &cache)
            .with_context(|| format!("app {:?}", cfg.name))?;
        check_imports(engine, &component).with_context(|| format!("app {:?}", cfg.name))?;
        let pre = linker
            .instantiate_pre(&component)
            .and_then(ServicePre::new)
            .map_err(anyhow::Error::from)
            .with_context(|| format!("linking app {:?}", cfg.name))?;
        let egress = match upstream {
            Some(u) => Route::Supervisor(u),
            None => Route::Direct(Arc::new(
                EgressPolicy::parse(&cfg.limits.egress).with_context(|| format!("app {:?} egress", cfg.name))?,
            )),
        };
        let static_dir = match &cfg.static_dir {
            Some(d) if !d.is_dir() => bail!("app {:?}: static_dir {} is not a directory", cfg.name, d.display()),
            // Reads through a shared directory fd don't interfere: every
            // lookup is `*at` relative to it, and listing opens a fresh fd.
            Some(d) => {
                let fd = std::fs::File::open(d).with_context(|| format!("opening static_dir {}", d.display()))?;
                Some(Dir::new(fd, FsPerms::ReadOnly, OpenMode::READ, true))
            }
            None => None,
        };
        Ok(App {
            name: cfg.name.as_str().into(),
            cfg: cfg.clone(),
            pre,
            egress,
            static_dir,
        })
    }
}
