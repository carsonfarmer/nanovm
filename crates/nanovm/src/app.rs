//! An app is one component plus its limits. Loading it compiles (or reuses a
//! cached compile), checks the component's imports against the capabilities
//! the node offers, and pre-links it so instantiation is cheap.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

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
    /// Its `wasi:config` values, shared by every object of the app: its
    /// config, and its secrets over it.
    pub config: Arc<WasiConfigVariables>,
    pub objects: Arc<Objects>,
    pub scratch: Arc<Scratch>,
    /// What its objects hold on disk, against its budget.
    pub disk: Arc<crate::quota::Disk>,
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
    Ok(())
}

/// Checks `component`'s imports and links it; a missing `wasi:http/handler`
/// export fails here, by name.
pub fn link(engine: &Engine, linker: &Linker<ObjState>, component: &Component) -> anyhow::Result<ServicePre<ObjState>> {
    check_imports(engine, component)?;
    Ok(linker.instantiate_pre(component).and_then(ServicePre::new)?)
}

/// Makes `objects` (an app's) if it isn't there and takes the app's
/// storage, which is this process's until the returned file closes (Q87).
pub(crate) fn lock(objects: &Path) -> anyhow::Result<std::fs::File> {
    std::fs::create_dir_all(objects)?;
    let lock = std::fs::File::open(objects)?;
    lock.try_lock().context("app storage is already owned or cannot be locked")?;
    Ok(lock)
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
        let ownership = lock(&objects)?;
        let component = crate::engine::load_component(engine, wasm, &cache)
            .with_context(|| format!("app {:?}", cfg.name))?;
        let pre = link(engine, linker, &component).with_context(|| format!("app {:?}", cfg.name))?;
        let egress = match upstream {
            Some(u) => Route::Supervisor(u),
            None => Route::Direct(Arc::new(
                EgressPolicy::parse(&cfg.limits.egress, &node.limits.egress, node.dev).with_context(|| format!("app {:?} egress", cfg.name))?,
            )),
        };
        let disk = Arc::new(crate::quota::Disk::open(&objects, node.app_disk_mb, node.disk_reserve_mb)?);
        Ok(App {
            name: cfg.name.as_str().into(),
            cfg: cfg.clone(),
            pre,
            egress,
            config: Arc::new(cfg.config.iter().chain(&cfg.secrets.0).collect()),
            objects: Arc::new(Objects::load(objects.canonicalize()?, node.max_objects_per_app, &cfg.limits, disk.reserve)?),
            scratch: Scratch::new(tmp)?,
            disk,
            _ownership: ownership,
        })
    }
}

/// Where object `id`'s directory is under `objects` (an app's): in one of
/// 256 shards, named by the first byte of the id's SHA-256, so no directory
/// holds more than a share of an app's objects (N-35). In it, `data` is the
/// volume (see `journal` for the rest).
pub fn object_dir(objects: &Path, id: &str) -> PathBuf {
    use sha2::Digest;
    objects.join(format!("{:02x}", sha2::Sha256::digest(id)[0])).join(id)
}

/// An app's objects on this node. An object has a directory only from its
/// first change on (see `journal`); these are counted, up to the node's
/// `max_objects_per_app`.
pub struct Objects {
    /// Where their directories are, without symlinks: the journal compares
    /// a volume's path with where its open files really are.
    pub dir: PathBuf,
    /// Empty directories, each the volume of an object that has none, until
    /// its first change moves it into the object's place.
    pub pool: Arc<Scratch>,
    count: AtomicU64,
    max: u64,
    /// The bytes and entries a new object may use: its quota.
    need: (u64, u64),
    /// The bytes the node keeps free (`quota::Disk`), which a new object's
    /// room is on top of.
    reserve: u64,
}

impl Objects {
    pub(crate) fn load(dir: PathBuf, max: u64, limits: &crate::config::Limits, reserve: u64) -> std::io::Result<Objects> {
        let mut count = 0;
        for shard in std::fs::read_dir(&dir)? {
            let shard = shard?;
            if shard.file_name().len() == 2 {
                count += std::fs::read_dir(shard.path())?.count() as u64;
            }
        }
        let pool = Scratch::new(dir.join("~new"))?;
        Ok(Objects { dir, pool, count: count.into(), max, need: (limits.disk_mb << 20, limits.max_files), reserve })
    }

    /// Whether a new object may be made: refused while the app has its
    /// most (`QuotaExceeded`, answered `507 object-limit`), or the disk has
    /// less free than the object's quota above the node's reserve
    /// (`StorageFull`, `507 disk-full`).
    pub fn room(&self) -> std::io::Result<()> {
        if self.count.load(Relaxed) >= self.max {
            return Err(self.most());
        }
        self.disk()
    }

    /// Takes a place for a new object, if there is [`room`](Self::room):
    /// at most `max` however many take one at once.
    pub fn admit(&self) -> std::io::Result<()> {
        self.disk()?;
        let more = self.count.try_update(Relaxed, Relaxed, |n| (n < self.max).then_some(n + 1));
        more.map(drop).map_err(|_| self.most())
    }

    fn disk(&self) -> std::io::Result<()> {
        let fs = rustix::fs::statvfs(&self.dir)?;
        if fs.f_bavail * fs.f_frsize < self.reserve + self.need.0 || (fs.f_files > 0 && fs.f_favail < self.need.1) {
            let (mb, files, reserve) = (self.need.0 >> 20, self.need.1, self.reserve as f64 / 1048576.0);
            let detail = format!("the disk has less free than a new object may use ({mb} MiB, {files} files) above the node's {reserve:.1} MiB reserve");
            return Err(std::io::Error::new(ErrorKind::StorageFull, detail));
        }
        Ok(())
    }

    /// An object's place, given back: it has no directory any more.
    /// Saturating: a restore (`ship::claim`) may make an object it never
    /// counted.
    pub fn release(&self) {
        self.count.update(Relaxed, Relaxed, |n| n.saturating_sub(1));
    }

    fn most(&self) -> std::io::Error {
        let detail = format!("the app has its most objects, {} (max_objects_per_app)", self.max);
        std::io::Error::new(ErrorKind::QuotaExceeded, detail)
    }

    /// Removes `base`, an object's directory, if its volume is empty or
    /// gone and it has no captured history (`committed`): an object that
    /// never committed a change, or emptied its volume, leaves nothing.
    pub fn settle(&self, base: &Path) -> std::io::Result<()> {
        let empty = match std::fs::read_dir(base.join(crate::journal::VOLUME)) {
            Ok(mut entries) => entries.next().is_none(),
            Err(e) if e.kind() == ErrorKind::NotFound => true,
            Err(e) => return Err(e),
        };
        if !empty || crate::committed::directory(base).try_exists()? {
            return Ok(());
        }
        match std::fs::remove_dir_all(base) {
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            removed => removed.map(|()| self.release()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn objects(name: &str, max: u64, need: (u64, u64)) -> Objects {
        let dir = std::env::temp_dir().join(format!("nanovm-objects-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let limits = crate::config::Limits { disk_mb: need.0, max_files: need.1, ..Default::default() };
        Objects::load(dir, max, &limits, 0).unwrap()
    }

    fn reason(e: std::io::Error) -> &'static str {
        match e.kind() {
            ErrorKind::QuotaExceeded => "object-limit",
            ErrorKind::StorageFull => "disk-full",
            _ => panic!("{e}"),
        }
    }

    /// Objects are counted from their directories; a place is taken only
    /// while there is room, however many threads ask at once; one given
    /// back makes room again; and a disk with less free than a new object's
    /// quota above the node's reserve has none.
    #[test]
    fn room_for_objects() {
        let o = objects("room", 0, (1, 1));
        for id in ["a", "b", "c"] {
            std::fs::create_dir_all(object_dir(&o.dir, id).join("data")).unwrap();
        }
        let dir = o.dir.clone();
        let o = Objects::load(dir.clone(), 8, &crate::config::Limits { disk_mb: 1, max_files: 1, ..Default::default() }, 0).unwrap();
        assert_eq!(o.count.load(Relaxed), 3, "counted, but not the pool");
        let admitted: usize = std::thread::scope(|s| {
            let threads: Vec<_> = (0..16).map(|_| s.spawn(|| o.admit().is_ok() as usize)).collect();
            threads.into_iter().map(|t| t.join().unwrap()).sum()
        });
        assert_eq!((admitted, o.count.load(Relaxed)), (5, 8));
        assert_eq!(reason(o.room().unwrap_err()), "object-limit");
        o.release();
        o.room().unwrap();
        // The quota above the node's reserve (`quota::Disk`).
        let o = Objects { reserve: u64::MAX / 2, ..o };
        assert_eq!(reason(o.room().unwrap_err()), "disk-full", "room within the node's reserve");
        let full = Objects { reserve: 0, need: (u64::MAX, 1), ..o };
        assert_eq!(reason(full.room().unwrap_err()), "disk-full");
        assert_eq!(reason(full.admit().unwrap_err()), "disk-full");
        assert_eq!(full.count.load(Relaxed), 7, "a refusal takes no place");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// An app's objects are spread over 256 shards, so 65,000 of them (a
    /// filesystem's subdirectory limit, N-35) put a few hundred in each
    /// directory, and an id always lands in the same one.
    #[test]
    fn objects_are_sharded() {
        let mut shards = std::collections::HashMap::<PathBuf, usize>::new();
        for n in 0..65_000 {
            let dir = object_dir(Path::new("/o"), &format!("user-{n}"));
            assert_eq!(dir.file_name().unwrap(), format!("user-{n}").as_str());
            assert_eq!(dir.parent().unwrap().parent().unwrap(), Path::new("/o"));
            *shards.entry(dir.parent().unwrap().to_path_buf()).or_default() += 1;
        }
        assert_eq!(shards.len(), 256);
        assert!(shards.values().all(|&n| n < 400), "{:?}", shards.values().max());
        assert_eq!(object_dir(Path::new("/o"), "a"), Path::new("/o/ca/a"));
    }

    /// An object goes when its volume is empty or gone and it has no
    /// captured history; any entry in its volume, or history, keeps it.
    #[test]
    fn only_empty_objects_go() {
        let o = objects("settle", 10, (0, 0));
        let base = |id| object_dir(&o.dir, id);
        let made = |id, files: &[&str]| {
            std::fs::create_dir_all(base(id).join("journal")).unwrap();
            std::fs::write(base(id).join("journal/log"), "log").unwrap();
            for f in files {
                let path = base(id).join(f);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, "x").unwrap();
            }
            o.count.fetch_add(1, Relaxed);
        };
        made("empty", &[]);
        std::fs::create_dir(base("empty").join("data")).unwrap();
        made("gone", &[]);
        made("file", &["data/f"]);
        made("dir", &[]);
        std::fs::create_dir_all(base("dir").join("data/sub")).unwrap();
        made("history", &["committed/00000000000000000001.tar"]);
        for id in ["empty", "gone", "file", "dir", "history", "never"] {
            o.settle(&base(id)).unwrap();
        }
        let left: Vec<_> = ["empty", "gone", "file", "dir", "history"].into_iter().filter(|id| base(id).exists()).collect();
        assert_eq!(left, ["file", "dir", "history"]);
        assert_eq!(o.count.load(Relaxed), 3);
        std::fs::remove_dir_all(&o.dir).unwrap();
    }
}
