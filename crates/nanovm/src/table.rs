//! The node: one loaded app, and the table mapping each object id to the
//! single actor that owns that object.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use bytes::Bytes;
use parking_lot::Mutex;
use tokio::sync::{Semaphore, mpsc, oneshot};
use wasmtime::Engine;
use wasmtime::component::Linker;

use crate::app::App;
use crate::config::{AppConfig, NodeConfig};
use crate::egress::Upstream;
use crate::metrics::Metrics;
use crate::object::{self, Job, ObjShared, ObjState, Phase, RespBody, error_response};

/// App, object and channel names: `[A-Za-z0-9_.-]{1,128}`, not starting
/// with `.`.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

struct Entry {
    /// Boxed: a channel allocates room for 32 jobs up front, which unboxed
    /// (about 600 bytes each) cost every live object 19 KB.
    tx: mpsc::Sender<Box<Job>>,
    shared: Arc<ObjShared>,
}

impl Entry {
    /// Since when the object has been idle with nothing queued and nothing
    /// waiting for room (a socket event holds a clone of `tx`). Only such an
    /// object can be evicted: its actor drains its queue before exiting, so
    /// evicting one with work would start a second actor beside it.
    fn idle_since(&self) -> Option<u64> {
        let (phase, since) = self.shared.phase();
        let empty = self.tx.capacity() == self.tx.max_capacity() && self.tx.strong_count() == 1;
        (phase == Phase::Idle && empty).then_some(since)
    }
}

/// An object id: a name not starting with `_`, which the node keeps for
/// itself (`/_nanovm/…`).
pub fn valid_object(s: &str) -> bool {
    valid_name(s) && !s.starts_with('_')
}

/// `/{object}/{rest}` as `(object, /rest)`, if the object id is valid.
pub fn route(path: &str) -> Option<(&str, &str)> {
    let path = path.strip_prefix('/')?;
    let (id, rest) = path.split_at(path.find('/').unwrap_or(path.len()));
    valid_object(id).then_some((id, rest))
}

pub const BAD_OBJECT: &str = "object ids are [A-Za-z0-9_.-]{1,128}, not starting with `.` or `_`";

pub struct Node {
    pub engine: Engine,
    pub cfg: NodeConfig,
    pub app: Arc<App>,
    pub metrics: Arc<Metrics>,
    pub telemetry: Arc<crate::telemetry::Telemetry>,
    /// Cores CPU-heavy requests may occupy at once; see `heavy_lanes`.
    pub heavy: Arc<Semaphore>,
    /// One permit per live store, released only after the store is dropped.
    pub slots: Arc<Semaphore>,
    /// An in-process node's WebSockets (`nanovm run`, tests); a jailed
    /// worker's are the proxy's.
    pub sockets: Arc<crate::ws::Sockets>,
    objects: Mutex<HashMap<Arc<str>, Entry>>,
}

impl Node {
    /// Builds a node running `app` from its component bytes. With
    /// `upstream`, guests' outbound requests go to the supervisor instead of
    /// the network. The caller has run `engine::preflight` (a jailed worker
    /// must, before it jails itself).
    pub fn new(cfg: NodeConfig, app: &AppConfig, wasm: &[u8], upstream: Option<Upstream>) -> anyhow::Result<Arc<Node>> {
        let engine = crate::engine::build(&cfg)?;
        let linker = crate::app::linker(&engine)?;
        Node::with_engine(engine, &linker, cfg, app, crate::engine::Wasm::Bytes(wasm), upstream)
    }

    /// [`Node::new`] with the engine and linker `engine::build` and
    /// `app::linker` made for `cfg`, which a worker makes before it is
    /// given its app.
    pub fn with_engine(
        engine: Engine,
        linker: &Linker<ObjState>,
        cfg: NodeConfig,
        app: &AppConfig,
        wasm: crate::engine::Wasm,
        upstream: Option<Upstream>,
    ) -> anyhow::Result<Arc<Node>> {
        crate::engine::start_ticker(&engine, Duration::from_millis(cfg.epoch_tick_ms));
        let telemetry = Arc::new(crate::telemetry::Telemetry::new(&cfg.telemetry_endpoint, &app.name, &app.digest, upstream.clone())?);
        let app = Arc::new(App::load(&engine, linker, &cfg, app, wasm, upstream)?);
        let heavy = Arc::new(Semaphore::new(cfg.heavy_lanes()));
        let slots = Arc::new(Semaphore::new(cfg.max_live_objects));
        let node = Arc::new(Node {
            engine,
            cfg,
            app,
            metrics: Arc::default(),
            telemetry,
            heavy,
            slots,
            sockets: Arc::new(crate::ws::Sockets::new(Default::default(), Default::default())),
            objects: Mutex::default(),
        });
        node.metrics.telemetry.set(Arc::downgrade(&node.telemetry)).ok();
        let started = Instant::now();
        let weak = Arc::downgrade(&node);
        node.telemetry.snapshots(move || {
            let Some(node) = weak.upgrade() else { return };
            let mut fields = vec![
                ("process.pid".into(), (std::process::id() as i64).into()),
                ("process.memory.usage".into(), (crate::metrics::rss_bytes() as i64).into()),
                ("process.uptime".into(), started.elapsed().as_secs_f64().into()),
                ("nanovm.stores.live".into(), node.metrics.stores_live.load(Relaxed).into()),
                ("nanovm.stores.peak".into(), node.metrics.stores_peak.load(Relaxed).into()),
                ("nanovm.objects.live".into(), (node.live_objects() as i64).into()),
                ("nanovm.objects.limit".into(), (node.cfg.max_live_objects as i64).into()),
            ];
            macro_rules! counters {
                ($($field:ident),* $(,)?) => { $(fields.push((concat!("nanovm.", stringify!($field)).into(),
                    (node.metrics.$field.load(Relaxed) as i64).into()));)* }
            }
            counters!(instantiate_failures, evicted_idle, evicted_lru, rejected_busy,
                rejected_capacity, egress_allowed, egress_denied, fs_denied, guest_cpu_ns);
            for p in crate::metrics::Poison::ALL {
                fields.push((format!("nanovm.poisons.{}", p.as_str()).into(), (node.metrics.poisons[p as usize].load(Relaxed) as i64).into()));
            }
            for (phase, count) in node.phases() {
                fields.push((format!("nanovm.objects.phase.{phase}").into(), (count as i64).into()));
            }
            node.telemetry.snapshot(fields);
        });
        Ok(node)
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

    pub fn live_objects(&self) -> usize {
        self.objects.lock().len()
    }

    /// Routes one request to its object's actor, starting the actor if the
    /// object isn't live, and waits for the response.
    pub async fn dispatch(self: &Arc<Self>, id: &str, req: http::Request<Bytes>) -> http::Response<RespBody> {
        if !valid_object(id) {
            return error_response(400, "bad-object-id", BAD_OBJECT);
        }
        use opentelemetry::propagation::TextMapPropagator;
        use opentelemetry::trace::{Span, TraceContextExt};
        let parent = req.extensions().get::<opentelemetry::Context>().cloned().unwrap_or_else(|| {
            opentelemetry_sdk::propagation::TraceContextPropagator::new()
                .extract(&opentelemetry_http::HeaderExtractor(req.headers()))
        });
        let mut span = self.telemetry.span("request", id, &parent, opentelemetry::trace::SpanKind::Server);
        span.set_attribute(opentelemetry::KeyValue::new("http.request.method", req.method().as_str().chars().take(32).collect::<String>()));
        let path = req.uri().path();
        span.set_attribute(opentelemetry::KeyValue::new("url.path", path[..path.floor_char_boundary(path.len().min(1024))].to_owned()));
        // The actor owns the recording span through post-return work. Only
        // immutable context is shared with guest logs and calls.
        let context = parent.with_remote_span_context(span.span_context().clone());
        let enqueued = Instant::now();
        let (reply, rx) = oneshot::channel();
        let job = Box::new(Job {
            req,
            reply,
            enqueued,
            span,
            context,
        });
        match self.enqueue(id, job) {
            Err(resp) => resp,
            Ok(()) => rx.await.unwrap_or_else(|_| error_response(503, "object-unavailable", "the object stopped before answering")),
        }
    }

    fn enqueue(self: &Arc<Self>, id: &str, mut job: Box<Job>) -> Result<(), http::Response<RespBody>> {
        let (app, key): (_, Arc<str>) = (&self.app, id.into());
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
                Err(mpsc::error::TrySendError::Full(mut job)) => {
                    self.metrics.rejected_busy.fetch_add(1, Relaxed);
                    let max = app.cfg.limits.max_queue;
                    let response = error_response(
                        503,
                        "object-busy",
                        &format!("queue full ({max} waiting)"),
                    );
                    crate::telemetry::response(&mut job.span, &response);
                    return Err(response);
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
            let response = error_response(
                503,
                "node-at-capacity",
                &format!("{max} live objects, none idle"),
            );
            crate::telemetry::response(&mut job.span, &response);
            return Err(response);
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

    /// Records that `from` waits on a call to object `to` of its app, unless
    /// `to` already waits on `from`, directly or through other calls: with
    /// one request at a time per object, that call could never be served.
    /// Checked and recorded under the table lock, so two objects calling each
    /// other at once can't both pass.
    pub fn wait_for(&self, from: &ObjShared, to: &str) -> bool {
        let objects = self.objects.lock();
        let (mut next, mut seen) = (vec![Arc::<str>::from(to)], std::collections::HashSet::new());
        while let Some(id) = next.pop() {
            if id == from.id {
                return false;
            }
            if seen.insert(id.clone())
                && let Some(e) = objects.get(&id)
            {
                next.extend(e.shared.waiting_on.lock().iter().cloned());
            }
        }
        from.waiting_on.lock().push(to.into());
        true
    }

    /// Drops the table's handle on the least recently used idle object; its
    /// actor drains its queue and exits.
    fn evict_lru(&self, objects: &mut HashMap<Arc<str>, Entry>) -> bool {
        let idle = objects.iter().filter_map(|(k, e)| Some((e.idle_since()?, k)));
        let Some(k) = idle.min().map(|(_, k)| k.clone()) else {
            return false;
        };
        let e = objects.remove(&k).unwrap();
        self.metrics.evicted_lru.fetch_add(1, Relaxed);
        self.metrics
            .event("evicted-lru", &e.shared.id, "node at max_live_objects", &opentelemetry::Context::new());
        true
    }

    /// Called by an actor whose instance went away. If nothing is queued for
    /// it, removes its table entry (if still its own) and tells it to exit;
    /// the table lock makes this atomic with respect to `enqueue`.
    pub fn try_retire(&self, shared: &Arc<ObjShared>, rx: &mpsc::Receiver<Box<Job>>) -> bool {
        let mut objects = self.objects.lock();
        if !rx.is_empty() {
            return false;
        }
        let key = &shared.id;
        if objects.get(key).is_some_and(|e| Arc::ptr_eq(&e.shared, shared)) {
            objects.remove(key);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Names are 1 to 128 of `[A-Za-z0-9_.-]`, never starting with `.`, so
    /// no name is `.`, `..`, hidden, or a path.
    #[test]
    fn names_are_safe_path_components() {
        for good in ["a", "A-z_0.9", "-", "_x", "a..b", &"x".repeat(128)] {
            assert!(valid_name(good), "{good:?}");
        }
        let long = "x".repeat(129);
        for bad in [
            "", ".", "..", ".hidden", "a/b", "a\\b", "a b", "é", "a\0", "a:b", "%2e", &long,
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    /// An idle object is evictable only with no work queued or waiting.
    #[test]
    fn only_objects_without_work_are_evictable() {
        use opentelemetry::trace::{Tracer, TracerProvider};
        let tracer = opentelemetry_sdk::trace::SdkTracerProvider::default().tracer("test");
        let job = || Box::new(Job {
            req: http::Request::new(Bytes::new()),
            reply: oneshot::channel().0,
            enqueued: Instant::now(),
            context: Default::default(),
            span: tracer.start("request"),
        });
        let (tx, _rx) = mpsc::channel(4);
        let e = Entry {
            tx,
            shared: Arc::new(ObjShared::new("app".into(), "o")),
        };
        assert_eq!(e.idle_since(), None, "a cold object is starting");
        e.shared.set_phase(Phase::Idle);
        assert!(e.idle_since().is_some());
        let waiting = e.tx.clone();
        assert_eq!(e.idle_since(), None, "an event waiting for room");
        drop(waiting);
        e.tx.try_send(job()).unwrap();
        assert_eq!(e.idle_since(), None, "a job not yet picked up");
    }

    /// A path is `/{object}` and what follows it; the object is a valid
    /// name not starting with `_`, so `//x` or `/_nanovm/…` name none.
    #[test]
    fn routes_name_one_object() {
        assert_eq!(route("/a"), Some(("a", "")));
        assert_eq!(route("/a/"), Some(("a", "/")));
        assert_eq!(route("/a/b/c?"), Some(("a", "/b/c?")));
        for bad in ["", "a", "/", "//a", "//a/b", "/_nanovm/stats", "/_x", "/../a", "/a b/c"] {
            assert_eq!(route(bad), None, "{bad:?}");
        }
    }
}
