//! An app is one component plus its limits. Loading it compiles (or reuses a
//! cached compile), checks the component's imports against the capabilities
//! the node offers, and pre-links it so instantiation is cheap.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use wasmtime::Engine;
use wasmtime::component::{Component, Linker};
use wasmtime_wasi_config::{WasiConfig, WasiConfigVariables};
use wasmtime_wasi_http::p3::bindings::ServicePre;

use crate::config::{AppConfig, NodeConfig};
use crate::egress::{EgressPolicy, Route, Upstream};
use crate::object::{ObjState, Scratch};

/// Import prefixes a guest may use. Everything else (sockets, the WASIp2
/// filesystem, anything custom) is refused at load time, and [`linker`]
/// defines none of it, so the only paths out of the sandbox are the quota'd
/// filesystem and allowlisted HTTP.
const ALLOWED_IMPORTS: &[&str] = &[
    "wasi:io/",
    "wasi:clocks/",
    "wasi:random/",
    "wasi:cli/",
    "wasi:filesystem/types@0.3",
    "wasi:filesystem/preopens@0.3",
    "wasi:http/types@0.3",
    "wasi:http/client@0.3",
    "wasi:config/store@0.2.0-rc.1",
];

pub struct App {
    pub name: Arc<str>,
    pub cfg: AppConfig,
    pub pre: ServicePre<ObjState>,
    pub egress: Route,
    /// Its `wasi:config` values, shared by every object of the app.
    pub config: Arc<WasiConfigVariables>,
    /// Where its objects' volumes are, without symlinks: the journal
    /// compares a volume's path with where its open files really are.
    pub objects: PathBuf,
    pub scratch: Arc<Scratch>,
    /// Advisory local exclusion, retained through every App owner's lifetime.
    _ownership: std::fs::File,
}

/// The host's side of every import [`ALLOWED_IMPORTS`] permits, and nothing
/// else: no WASIp2 filesystem (it would reach `/data` around the quota and
/// the journal) or sockets, so an allowlist that let them through would
/// still fail to link them (N-500).
pub fn linker(engine: &Engine) -> anyhow::Result<Linker<ObjState>> {
    use wasmtime_wasi::p2::bindings::{cli, random};
    use wasmtime_wasi::{WasiView, cli::WasiCli, cli::WasiCliView, random::WasiRandom};
    let mut linker = Linker::<ObjState>::new(engine);
    // WASIp2 io, clocks, `random` and stdio; then the rest of random and cli.
    wasmtime_wasi::p2::add_to_linker_proxy_interfaces_async(&mut linker)?;
    let l = &mut linker;
    random::insecure::add_to_linker::<_, WasiRandom>(l, |s| s.ctx().ctx.random())?;
    random::insecure_seed::add_to_linker::<_, WasiRandom>(l, |s| s.ctx().ctx.random())?;
    cli::exit::add_to_linker::<_, WasiCli>(l, ObjState::cli)?;
    cli::environment::add_to_linker::<_, WasiCli>(l, ObjState::cli)?;
    cli::terminal_input::add_to_linker::<_, WasiCli>(l, ObjState::cli)?;
    cli::terminal_output::add_to_linker::<_, WasiCli>(l, ObjState::cli)?;
    cli::terminal_stdin::add_to_linker::<_, WasiCli>(l, ObjState::cli)?;
    cli::terminal_stdout::add_to_linker::<_, WasiCli>(l, ObjState::cli)?;
    cli::terminal_stderr::add_to_linker::<_, WasiCli>(l, ObjState::cli)?;
    crate::quota::add_to_linker(&mut linker)?;
    wasmtime_wasi_config::add_to_linker(&mut linker, |s| WasiConfig::from(&*s.config))?;
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
        let [objects, tmp, cache] = node.app_dirs(&cfg.name);
        std::fs::create_dir_all(&objects)?;
        let ownership = std::fs::File::open(&objects)?;
        ownership.try_lock().context("app storage is already owned or cannot be locked")?;
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
                EgressPolicy::parse(&cfg.limits.egress, node.dev).with_context(|| format!("app {:?} egress", cfg.name))?,
            )),
        };
        Ok(App {
            name: cfg.name.as_str().into(),
            cfg: cfg.clone(),
            pre,
            egress,
            config: Arc::new(cfg.config.iter().collect()),
            objects: objects.canonicalize()?,
            scratch: Scratch::new(tmp)?,
            _ownership: ownership,
        })
    }
}
