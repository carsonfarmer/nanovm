//! The node: loaded apps, and the table mapping `(app, object-id)` to the
//! single actor that owns that object.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use bytes::Bytes;
use parking_lot::Mutex;
use tokio::sync::{Semaphore, mpsc, oneshot};
use wasmtime::Engine;

use crate::app::App;
use anyhow::Context;

use crate::config::NodeConfig;
use crate::egress::Upstream;
use crate::metrics::Metrics;
use crate::object::{self, Job, ObjShared, Phase, RespBody, error_response};

/// App and object names: `[A-Za-z0-9_.-]{1,128}`, not starting with `.`.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

struct Entry {
    tx: mpsc::Sender<Job>,
    shared: Arc<ObjShared>,
}

pub struct Node {
    pub engine: Engine,
    pub cfg: NodeConfig,
    pub apps: HashMap<String, Arc<App>>,
    pub metrics: Arc<Metrics>,
    pub started: Instant,
    /// Cores CPU-heavy requests may occupy at once; see `heavy_lanes`.
    pub heavy: Arc<Semaphore>,
    /// One permit per live store, released only after the store is dropped.
    pub slots: Arc<Semaphore>,
    objects: Mutex<HashMap<(Arc<str>, Arc<str>), Entry>>,
    seq: AtomicU64,
}

impl Node {
    /// Builds a node from the components named in `cfg`, with direct egress.
    pub fn new(cfg: NodeConfig) -> anyhow::Result<Arc<Node>> {
        crate::engine::preflight(&cfg)?;
        let mut wasm = Vec::new();
        for a in &cfg.apps {
            wasm.push(std::fs::read(&a.component).with_context(|| format!("reading {}", a.component.display()))?);
        }
        Node::from_wasm(cfg, wasm, None)
    }

    /// Builds a node from component bytes, one per app in `cfg.apps`. With
    /// `upstream`, guests' outbound requests go to the supervisor instead of
    /// the network.
    /// The caller has run `engine::preflight` (a jailed worker must, before
    /// it jails itself).
    pub fn from_wasm(cfg: NodeConfig, wasm: Vec<Vec<u8>>, upstream: Option<Upstream>) -> anyhow::Result<Arc<Node>> {
        let engine = crate::engine::build(&cfg)?;
        crate::engine::start_ticker(&engine, Duration::from_millis(cfg.epoch_tick_ms));
        let linker = crate::app::linker(&engine)?;
        let mut apps = HashMap::new();
        for (a, wasm) in cfg.apps.iter().zip(wasm) {
            let app = App::load(&engine, &linker, &cfg, a, &wasm, upstream.clone())?;
            apps.insert(a.name.clone(), Arc::new(app));
            // Scratch from a previous run is garbage; volumes are kept. The
            // directory itself stays: a jailed worker's Landlock rule names it.
            let [_, tmp, _] = cfg.app_dirs(&a.name);
            for e in std::fs::read_dir(&tmp).into_iter().flatten().flatten() {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        let heavy = Arc::new(Semaphore::new(cfg.heavy_lanes()));
        let slots = Arc::new(Semaphore::new(cfg.max_live_objects));
        Ok(Arc::new(Node {
            engine,
            cfg,
            apps,
            metrics: Arc::default(),
            started: Instant::now(),
            heavy,
            slots,
            objects: Mutex::default(),
            seq: AtomicU64::new(1),
        }))
    }

    /// Releases every object and waits until all their stores are gone.
    /// Each actor holds the node alive, so dropping the last outside handle
    /// frees nothing until this has run.
    pub async fn shutdown(&self) {
        // Dropping each queue's sender lets its actor drain and exit.
        self.objects.lock().clear();
        while self.metrics.stores_live.load(Relaxed) > 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Relaxed)
    }

    pub fn live_objects(&self) -> usize {
        self.objects.lock().len()
    }

    /// Routes one request to its object's actor, starting the actor if the
    /// object isn't live, and waits for the response.
    pub async fn dispatch(
        self: &Arc<Self>,
        app: &Arc<App>,
        id: &str,
        req: http::Request<Bytes>,
    ) -> http::Response<RespBody> {
        if !valid_name(id) {
            return error_response(400, "bad-object-id", "object ids are [A-Za-z0-9_.-]{1,128}");
        }
        let (reply, rx) = oneshot::channel();
        let job = Job {
            req,
            reply,
            enqueued: Instant::now(),
        };
        if let Err(resp) = self.enqueue(app, id, job) {
            return resp;
        }
        match rx.await {
            Ok(resp) => resp,
            Err(_) => error_response(503, "object-unavailable", "the object stopped before answering"),
        }
    }

    fn enqueue(self: &Arc<Self>, app: &Arc<App>, id: &str, mut job: Job) -> Result<(), http::Response<RespBody>> {
        let key: (Arc<str>, Arc<str>) = (app.name.clone(), id.into());
        let mut objects = self.objects.lock();
        if let Some(e) = objects.get(&key) {
            match e.tx.try_send(job) {
                Ok(()) => return Ok(()),
                // A socket's event waits instead: each socket has one at a
                // time, so the sockets bound them.
                Err(mpsc::error::TrySendError::Full(job)) if crate::ws::is_event(&job.req) => {
                    let tx = e.tx.clone();
                    tokio::spawn(async move { tx.send(job).await });
                    return Ok(());
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.metrics.rejected_busy.fetch_add(1, Relaxed);
                    let max = app.cfg.limits.max_queue;
                    return Err(error_response(
                        503,
                        "object-busy",
                        &format!("queue full ({max} waiting)"),
                    ));
                }
                Err(mpsc::error::TrySendError::Closed(j)) => {
                    objects.remove(&key);
                    job = j;
                }
            }
        }
        if objects.len() >= self.cfg.max_live_objects && !self.evict_lru(&mut objects) {
            self.metrics.rejected_capacity.fetch_add(1, Relaxed);
            let max = self.cfg.max_live_objects;
            return Err(error_response(
                503,
                "node-at-capacity",
                &format!("{max} live objects, none idle"),
            ));
        }
        let shared = Arc::new(ObjShared::new(app.name.clone(), id));
        let (tx, rx) = mpsc::channel(app.cfg.limits.max_queue.max(1));
        tx.try_send(job).ok().expect("fresh queue has room");
        objects.insert(
            key,
            Entry {
                tx,
                shared: shared.clone(),
            },
        );
        drop(objects);
        tokio::spawn(object::run(self.clone(), app.clone(), shared, rx));
        Ok(())
    }

    /// Drops the table's handle on the least recently used idle object; its
    /// actor drains its queue and exits.
    fn evict_lru(&self, objects: &mut HashMap<(Arc<str>, Arc<str>), Entry>) -> bool {
        let idle = objects.iter().filter_map(|(k, e)| match e.shared.phase() {
            (Phase::Idle, since) => Some((since, k)),
            _ => None,
        });
        let Some(k) = idle.min().map(|(_, k)| k.clone()) else {
            return false;
        };
        let e = objects.remove(&k).unwrap();
        self.metrics.evicted_lru.fetch_add(1, Relaxed);
        self.metrics
            .event("evicted-lru", &e.shared.app, &e.shared.id, "node at max_live_objects");
        true
    }

    /// Called by an actor whose instance went away. If nothing is queued for
    /// it, removes its table entry (if still its own) and tells it to exit;
    /// the table lock makes this atomic with respect to `enqueue`.
    pub fn try_retire(&self, shared: &Arc<ObjShared>, rx: &mpsc::Receiver<Job>) -> bool {
        let mut objects = self.objects.lock();
        if !rx.is_empty() {
            return false;
        }
        let key = (shared.app.clone(), shared.id.clone());
        if objects.get(&key).is_some_and(|e| Arc::ptr_eq(&e.shared, shared)) {
            objects.remove(&key);
        }
        true
    }

    /// How many live objects are in each phase.
    pub fn phases(&self) -> HashMap<&'static str, usize> {
        let mut phases = HashMap::new();
        for e in self.objects.lock().values() {
            *phases.entry(e.shared.phase().0.as_str()).or_default() += 1;
        }
        phases
    }
}
