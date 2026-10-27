//! The node: loaded apps, and the table mapping `(app, object-id)` to the
//! single actor that owns that object.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use bytes::Bytes;
use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::{Semaphore, mpsc, oneshot};
use wasmtime::Engine;

use crate::app::App;
use crate::config::NodeConfig;
use crate::metrics::{Metrics, now_ns};
use crate::object::{self, Job, ObjShared, Phase, RespBody, error_response};
use crate::quota::Usage;

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
    next_gen: AtomicU64,
    seq: AtomicU64,
    /// Generations of objects retired after a kill, for `grid`.
    recent_kills: Mutex<VecDeque<(u64, u64)>>,
}

/// How long `grid` keeps showing a killed object.
const KILL_SHOW_NS: u64 = 3_000_000_000;

#[derive(Serialize)]
pub struct ObjInfo {
    pub app: Arc<str>,
    pub id: Arc<str>,
    pub phase: &'static str,
    pub phase_ms: u64,
    pub requests: u64,
    pub instantiations: u64,
    pub poisons: u64,
    pub cpu_us: u64,
    pub idle_ms: u64,
    pub last_poison: Option<&'static str>,
    pub last_poison_ago_ms: Option<u64>,
    pub disk: Vec<Usage>,
}

impl Node {
    pub fn new(cfg: NodeConfig) -> anyhow::Result<Arc<Node>> {
        crate::engine::preflight(&cfg)?;
        let engine = crate::engine::build(&cfg)?;
        crate::engine::start_ticker(&engine, Duration::from_millis(cfg.epoch_tick_ms));
        let linker = crate::app::linker(&engine)?;
        let mut apps = HashMap::new();
        for a in &cfg.apps {
            apps.insert(a.name.clone(), Arc::new(App::load(&engine, &linker, &cfg, a)?));
        }
        // Scratch from a previous run is garbage; volumes are kept.
        let _ = std::fs::remove_dir_all(cfg.data_dir.join("tmp"));
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
            next_gen: AtomicU64::new(1),
            seq: AtomicU64::new(1),
            recent_kills: Mutex::default(),
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
        app: &str,
        id: &str,
        req: http::Request<Bytes>,
    ) -> http::Response<RespBody> {
        let Some(app) = self.apps.get(app) else {
            return error_response(404, "no-such-app", app);
        };
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
        let shared = Arc::new(ObjShared::new(
            app.name.clone(),
            id,
            self.next_gen.fetch_add(1, Relaxed),
        ));
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
        let victim = objects
            .iter()
            .filter(|(_, e)| e.shared.phase().0 == Phase::Idle)
            .min_by_key(|(_, e)| e.shared.last_used_ns.load(Relaxed))
            .map(|(k, _)| k.clone());
        match victim {
            Some(k) => {
                let e = objects.remove(&k).unwrap();
                self.metrics.evicted_lru.fetch_add(1, Relaxed);
                self.metrics
                    .event("evicted-lru", &e.shared.app, &e.shared.id, "node at max_live_objects");
                true
            }
            None => false,
        }
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
            let now = now_ns();
            if shared
                .last_poison
                .lock()
                .is_some_and(|(_, t)| now.saturating_sub(t) < KILL_SHOW_NS)
            {
                let mut kills = self.recent_kills.lock();
                if kills.len() == 4096 {
                    kills.pop_front();
                }
                kills.push_back((shared.generation, now));
            }
        }
        true
    }

    /// One char per live object, oldest first: the initial of its phase
    /// (`c`old, `s`tarting, `i`dle, `h`andling, `p`ost-return), uppercased if
    /// it was poisoned in the last 3 s, plus `X` for objects retired after a
    /// kill in the last 3 s. Cheap enough to poll with 20k objects.
    pub fn grid(&self) -> String {
        let now = now_ns();
        let mut all: Vec<(u64, u8)> = self
            .objects
            .lock()
            .values()
            .map(|e| {
                let s = &e.shared;
                let c = s.phase().0.as_str().as_bytes()[0];
                let hot = s
                    .last_poison
                    .lock()
                    .is_some_and(|(_, t)| now.saturating_sub(t) < KILL_SHOW_NS);
                (s.generation, if hot { c.to_ascii_uppercase() } else { c })
            })
            .collect();
        let mut kills = self.recent_kills.lock();
        while kills
            .front()
            .is_some_and(|(_, t)| now.saturating_sub(*t) >= KILL_SHOW_NS)
        {
            kills.pop_front();
        }
        all.extend(kills.iter().map(|(g, _)| (*g, b'X')));
        drop(kills);
        all.sort_unstable_by_key(|x| x.0);
        all.into_iter().map(|x| x.1 as char).collect()
    }

    /// The most recently used live objects, newest first.
    pub fn objects(&self, limit: usize) -> (Vec<ObjInfo>, HashMap<&'static str, usize>) {
        let now = now_ns();
        let all: Vec<Arc<ObjShared>> = self.objects.lock().values().map(|e| e.shared.clone()).collect();
        let mut phases: HashMap<&'static str, usize> = HashMap::new();
        for s in &all {
            *phases.entry(s.phase().0.as_str()).or_default() += 1;
        }
        let mut all = all;
        all.sort_unstable_by_key(|s| std::cmp::Reverse(s.last_used_ns.load(Relaxed)));
        all.truncate(limit);
        let ms = |ns: u64| ns / 1_000_000;
        let infos = all
            .iter()
            .map(|s| {
                let (phase, since) = s.phase();
                let last = *s.last_poison.lock();
                ObjInfo {
                    app: s.app.clone(),
                    id: s.id.clone(),
                    phase: phase.as_str(),
                    phase_ms: ms(now.saturating_sub(since)),
                    requests: s.requests.load(Relaxed),
                    instantiations: s.instantiations.load(Relaxed),
                    poisons: s.poisons.load(Relaxed),
                    cpu_us: s.cpu_total_ns.load(Relaxed) / 1000,
                    idle_ms: ms(now.saturating_sub(s.last_used_ns.load(Relaxed))),
                    last_poison: last.map(|(p, _)| p.as_str()),
                    last_poison_ago_ms: last.map(|(_, t)| ms(now.saturating_sub(t))),
                    disk: s.quota.lock().as_ref().map(|q| q.usage()).unwrap_or_default(),
                }
            })
            .collect();
        (infos, phases)
    }
}
