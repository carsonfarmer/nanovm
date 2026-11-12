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
    bound_request_heads(&mut linker)?;
    Ok(linker)
}

/// A guest's request holds its method, scheme, authority and path on the
/// host beside its headers, and their setters take any length. In their
/// place: the same setters, refusing (with their WIT error) what would take
/// that head past `MAX_HEAD`, as the node refuses a head it is sent. So a
/// resource still holds at most `MAX_HEAD` (N-75, N-691).
fn bound_request_heads(linker: &mut Linker<ObjState>) -> anyhow::Result<()> {
    use wasmtime::component::{ComponentType, Lift, LinkerInstance, Resource};
    use wasmtime_wasi_http::p3::{Request, bindings::http::types::{HostRequest, Method, Scheme}};
    use wasmtime_wasi_http::{WasiHttpCtxView, WasiHttpView};
    // The parts of a request's head besides its headers.
    fn method(r: &Request) -> usize { r.method.as_str().len() }
    fn scheme(r: &Request) -> usize { r.scheme.as_ref().map_or(0, |s| s.as_str().len()) }
    fn authority(r: &Request) -> usize { r.authority.as_ref().map_or(0, |a| a.as_str().len()) }
    fn path(r: &Request) -> usize { r.path_with_query.as_ref().map_or(0, |p| p.as_str().len()) }
    /// About what `r`'s head holds, its headers counted as `FieldMap` does.
    fn head(r: &Request) -> usize {
        let names = r.headers.keys().map(|k| k.as_str().len() + size_of::<http::HeaderName>());
        let values = r.headers.values().map(|v| v.len() + size_of::<http::HeaderValue>());
        names.chain(values).sum::<usize>() + method(r) + scheme(r) + authority(r) + path(r)
    }
    type Set<P> = fn(&mut WasiHttpCtxView<'_>, Resource<Request>, P) -> wasmtime::Result<Result<(), ()>>;
    /// Shadows the setter `name` of the part `old` measures, which `new`
    /// measures in the value it is given.
    fn bound<P: ComponentType + Lift + 'static>(
        types: &mut LinkerInstance<'_, ObjState>,
        name: &str,
        old: fn(&Request) -> usize,
        new: fn(&P) -> usize,
        set: Set<P>,
    ) -> wasmtime::Result<()> {
        types.func_wrap(name, move |mut s, (req, value): (Resource<Request>, P)| {
            let r = s.data().table.get(&req)?;
            if head(r) - old(r) + new(&value) > crate::server::MAX_HEAD {
                return Ok((Err(()),));
            }
            Ok((set(&mut s.data_mut().http(), req, value)?,))
        })
    }
    let other = |m: &Method| if let Method::Other(m) = m { m.len() } else { 0 };
    let other_scheme = |s: &Option<Scheme>| if let Some(Scheme::Other(s)) = s { s.len() } else { 0 };
    let string = |s: &Option<String>| s.as_ref().map_or(0, String::len);
    linker.allow_shadowing(true);
    let mut t = linker.instance("wasi:http/types@0.3.0")?;
    bound(&mut t, "[method]request.set-method", method, other, |h, r, v| h.set_method(r, v))?;
    bound(&mut t, "[method]request.set-scheme", scheme, other_scheme, |h, r, v| h.set_scheme(r, v))?;
    bound(&mut t, "[method]request.set-authority", authority, string, |h, r, v| h.set_authority(r, v))?;
    bound(&mut t, "[method]request.set-path-with-query", path, string, |h, r, v| h.set_path_with_query(r, v))?;
    linker.allow_shadowing(false);
    Ok(())
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
        wasm: crate::engine::Wasm,
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
