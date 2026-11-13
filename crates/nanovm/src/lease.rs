//! Leases: which node serves each object, when there are several (W-10).
//!
//! The proxy places each object on a home node, but placement alone can't
//! keep an object to one live instance: nodes stop, and proxies may disagree
//! about the list. So before a node instantiates an object it takes the
//! object's lease, a record in an object store naming the node and an epoch
//! one past the last; a node that doesn't hold the lease refuses the object's
//! requests (421, naming the holder), and the proxy sends them there. A node
//! commits an object's changes only while it holds the lease, and the commit
//! carries the epoch (see `object::ObjShared::lease_epoch`). With no store there is
//! one node, and every lease is its own at epoch 0.
//!
//! Every write is conditional on the version read, so of two nodes racing for
//! a lease one wins. A node renews one record for all its leases, its
//! session, every `lease_ms / 4`; they last until 7/8 of `lease_ms` after it
//! sent the last renewal that succeeded, after which it serves none of their
//! objects and starts a new session. Another node takes an object whose
//! holder's session record it has seen unchanged for the holder's `lease_ms`
//! (which the record carries, so nodes needn't agree on it), by its own
//! clock (clocks needn't agree, only tick at about the same rate). It first
//! ends that session with a conditional write, so a renewal still on its way
//! can't land after it and extend the holder's leases.
//!
//! Records: `nodes/<node>` is `{session, beat, lease_ms}` and
//! `objects/<app>/<object>` `{node, session, epoch}`, with no node once
//! released. Deleting a record restarts its epochs: never do.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, UpdateVersion};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Where a worker asks its supervisor for leases: `POST` takes one, `DELETE`
/// gives it up.
pub const PATH: &str = "/_lease/";
/// Marks a node's refusal (421) of a request for an object it doesn't hold,
/// naming the node that does, or empty.
pub const OWNER: &str = "nanovm-owner";
/// The supervisor's word on its session to a worker: `<id> <until>`.
pub const SESSION: &str = "nanovm-session";

/// This host's monotonic clock in nanoseconds: the same in the supervisor
/// and its workers, which an `Instant` can't pass between.
pub fn mono_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid out-pointer and the clock always exists.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// A node's lease on one object.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Grant {
    pub epoch: u64,
    /// The node's session it was taken in.
    pub session: u64,
    /// The version of the lease record it wrote.
    pub e_tag: Option<String>,
    pub version: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct NodeRecord {
    session: u64,
    beat: u64,
    /// The node's `lease_ms`: how long another waits, seeing the record
    /// unchanged, before it takes the node's objects. The holder's, not the
    /// taker's, so nodes needn't agree on it (0, from an older node: the
    /// taker's).
    #[serde(default)]
    lease_ms: u64,
}

#[derive(Serialize, Deserialize)]
struct ObjectRecord {
    node: String,
    session: u64,
    epoch: u64,
}

/// Why a lease wasn't granted.
#[derive(Debug, PartialEq)]
pub enum Refused {
    /// Another node's session holds it.
    Held(String),
    /// The store can't be reached, or this node's session has lapsed.
    Unavailable(String),
}

/// This node's session, as its supervisor renews it.
#[derive(Clone, Default)]
struct Own {
    id: u64,
    beat: u64,
    version: Option<UpdateVersion>,
    /// `mono_ns` until which its leases hold.
    until: u64,
}

/// A supervisor's leases, in an object store shared by every node.
pub struct Leases {
    store: Arc<dyn ObjectStore>,
    node: String,
    ttl: Duration,
    session: Mutex<Own>,
    /// Other nodes' session records: the version seen, and since when.
    seen: Mutex<HashMap<String, (Option<UpdateVersion>, Instant)>>,
    /// The grants this node's workers hold, by key, each given out once its
    /// object was claimed (see `serve`): what the node may ship.
    held: Mutex<HashMap<String, Grant>>,
}

type Result<T> = object_store::Result<T>;

impl Leases {
    pub fn new(store: Arc<dyn ObjectStore>, node: &str, ttl: Duration) -> Leases {
        Leases { store, node: node.into(), ttl, session: Mutex::default(), seen: Mutex::default(), held: Mutex::default() }
    }

    /// The store an object store URL names, if any.
    pub fn open(url: &str) -> anyhow::Result<Option<Arc<dyn ObjectStore>>> {
        if url.is_empty() {
            return Ok(None);
        }
        let (store, prefix) = object_store::parse_url_opts(&url.parse()?, std::env::vars())?;
        Ok(Some(Arc::new(object_store::prefix::PrefixStore::new(store, prefix))))
    }

    /// Renews this node's session, starting a new one if it has lapsed: its
    /// id, and the `mono_ns` until which its leases hold (past if none do).
    pub async fn renew(&self) -> (u64, u64) {
        let mut s = self.session.lock().clone();
        let (sent, path) = (mono_ns(), Path::from_iter(["nodes", &self.node]));
        if s.until <= sent {
            match self.get::<NodeRecord>(&path).await {
                Ok((rec, version)) => s = Own { id: rec.map_or(1, |r| r.session + 1), beat: 0, version, until: 0 },
                Err(_) => return (s.id, s.until),
            }
        }
        let rec = NodeRecord { session: s.id, beat: s.beat + 1, lease_ms: self.ttl.as_millis() as u64 };
        match self.put(&path, &rec, s.version.clone()).await {
            Ok(v) => {
                (s.beat, s.version) = (s.beat + 1, Some(v));
                s.until = sent + (self.ttl * 7 / 8).as_nanos() as u64;
            }
            // Our last renewal may have landed unanswered: carry on from it,
            // but not past the lease it was sent for. Anything else ended
            // the session.
            Err(object_store::Error::Precondition { .. }) => {
                if let Ok((Some(r), v)) = self.get::<NodeRecord>(&path).await
                    && r.session == s.id
                {
                    (s.beat, s.version) = (r.beat, v);
                }
            }
            Err(_) => {}
        }
        *self.session.lock() = s.clone();
        (s.id, s.until)
    }

    /// Takes the lease on `key` (`<app>/<object>`), at the next epoch,
    /// unless another node's live session holds it. The grant is in the
    /// session this began in, so only while that is still this node's and
    /// live when the store has answered: it can have ended meanwhile, and
    /// the object been taken over.
    pub async fn acquire(&self, key: &str) -> std::result::Result<Grant, Refused> {
        let Own { id, until, .. } = self.session.lock().clone();
        if until <= mono_ns() {
            return Err(Refused::Unavailable("this node's session has lapsed".into()));
        }
        let path = Path::from_iter(["objects"].into_iter().chain(key.split('/')));
        let unavailable = |e: object_store::Error| Refused::Unavailable(e.to_string());
        for _ in 0..4 {
            let (rec, version) = self.get::<ObjectRecord>(&path).await.map_err(unavailable)?;
            if let Some(r) = &rec
                && !r.node.is_empty()
                && r.node != self.node
                && self.alive(&r.node, r.session).await.map_err(unavailable)?
            {
                return Err(Refused::Held(r.node.clone()));
            }
            let epoch = rec.map_or(1, |r| r.epoch + 1);
            let grant = match self.put(&path, &ObjectRecord { node: self.node.clone(), session: id, epoch }, version).await {
                Ok(v) => Grant { epoch, session: id, e_tag: v.e_tag, version: v.version },
                Err(object_store::Error::Precondition { .. } | object_store::Error::AlreadyExists { .. }) => continue,
                Err(e) => return Err(unavailable(e)),
            };
            // Any grant this node held on it before is over.
            self.held.lock().remove(key);
            return match self.until(&grant) {
                Some(_) => Ok(grant),
                None => Err(Refused::Unavailable("this node's session ended while it took the lease".into())),
            };
        }
        Err(Refused::Unavailable(format!("{key}'s lease keeps changing")))
    }

    /// The `mono_ns` until which `grant` holds: that of the session it was
    /// taken in, while that is this node's and live.
    pub fn until(&self, grant: &Grant) -> Option<u64> {
        let s = self.session.lock();
        (s.id == grant.session && mono_ns() < s.until).then_some(s.until)
    }

    /// This node's name.
    pub fn node(&self) -> &str {
        &self.node
    }

    /// Records that this node's worker holds `grant` on `key`.
    pub fn hold(&self, key: &str, grant: Grant) {
        self.held.lock().insert(key.into(), grant);
    }

    fn unhold(&self, key: &str, grant: &Grant) {
        let mut held = self.held.lock();
        if held.get(key).is_some_and(|g| g.epoch == grant.epoch) {
            held.remove(key);
        }
    }

    /// The epoch of the lease this node holds on `key`, live: held by a
    /// worker, and in this node's session, which hasn't lapsed.
    pub fn epoch(&self, key: &str) -> Option<u64> {
        let grant = self.held.lock().get(key).cloned()?;
        self.until(&grant).map(|_| grant.epoch)
    }

    /// Gives up the lease `grant` took on `key`, unless it has moved on.
    pub async fn release(&self, key: &str, grant: &Grant) {
        self.unhold(key, grant);
        let path = Path::from_iter(["objects"].into_iter().chain(key.split('/')));
        let version = UpdateVersion { e_tag: grant.e_tag.clone(), version: grant.version.clone() };
        let released = ObjectRecord { node: String::new(), session: 0, epoch: grant.epoch };
        let _ = self.put(&path, &released, Some(version)).await;
    }

    /// Whether `node`'s session `session` may still hold leases: its record
    /// names it and has changed within a lease. Once it hasn't, this ends it.
    async fn alive(&self, node: &str, session: u64) -> Result<bool> {
        let path = Path::from_iter(["nodes", node]);
        let (rec, version) = self.get::<NodeRecord>(&path).await?;
        let Some(rec) = rec.filter(|r| r.session == session) else { return Ok(false) };
        {
            let mut seen = self.seen.lock();
            let (seen_version, since) = seen.entry(node.into()).or_insert((None, Instant::now()));
            if *seen_version != version {
                (*seen_version, *since) = (version.clone(), Instant::now());
            }
            if since.elapsed() < Some(rec.lease_ms).filter(|&ms| ms > 0).map_or(self.ttl, Duration::from_millis) {
                return Ok(true);
            }
        }
        match self.put(&path, &NodeRecord { session: session + 1, beat: 0, ..rec }, version).await {
            Ok(_) => Ok(false),
            Err(object_store::Error::Precondition { .. }) => Ok(true),
            Err(e) => Err(e),
        }
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &Path) -> Result<(Option<T>, Option<UpdateVersion>)> {
        let res = match self.store.get(path).await {
            Err(object_store::Error::NotFound { .. }) => return Ok((None, None)),
            res => res?,
        };
        let version = UpdateVersion { e_tag: res.meta.e_tag.clone(), version: res.meta.version.clone() };
        let rec = serde_json::from_slice(&res.bytes().await?)
            .map_err(|e| object_store::Error::Generic { store: "lease", source: e.into() })?;
        Ok((Some(rec), Some(version)))
    }

    /// Writes `rec` at `path` if it is still at `version` (absent for none).
    async fn put(&self, path: &Path, rec: &impl Serialize, version: Option<UpdateVersion>) -> Result<UpdateVersion> {
        let mode = version.map_or(PutMode::Create, PutMode::Update);
        let body = serde_json::to_vec(rec).expect("records serialize");
        let res = self.store.put_opts(path, body.into(), mode.into()).await?;
        Ok(UpdateVersion { e_tag: res.e_tag, version: res.version })
    }
}

/// A worker's view of its node's session, as the supervisor last told it:
/// its id, and the `mono_ns` until which its leases hold.
#[derive(Default)]
pub struct Session(Mutex<(u64, u64)>);

impl Session {
    /// Records what the supervisor says of its session, unless it has said
    /// more already (a later session, or this one renewed later): what it
    /// says can arrive out of order. Whether the session is now a new one,
    /// or has lapsed.
    pub fn set(&self, id: u64, until: u64) -> bool {
        let mut s = self.0.lock();
        let old = s.0;
        *s = (*s).max((id, until));
        old != s.0 || s.1 <= mono_ns()
    }

    /// Whether `grant` is in the node's session and that hasn't lapsed.
    pub fn holds(&self, grant: &Grant) -> bool {
        let (id, until) = *self.0.lock();
        grant.session == id && mono_ns() < until
    }
}

/// A worker's side: its node's session, and the link to ask its supervisor
/// for leases.
pub struct Worker {
    pub session: Arc<Session>,
    up: crate::egress::Upstream,
}

impl Worker {
    pub fn new(up: crate::egress::Upstream) -> Worker {
        Worker { session: Arc::default(), up }
    }

    /// Asks the supervisor for `object`'s lease: the grant, with the node's
    /// session and its `until`; else the node holding it, if one does.
    pub async fn acquire(&self, object: &str) -> std::result::Result<(Grant, u64, u64), Option<String>> {
        let (status, owner, body) = self.ask(http::Method::POST, object, bytes::Bytes::new()).await.ok_or(None)?;
        match status {
            200 => serde_json::from_slice(&body).map_err(|_| None),
            _ => Err(owner),
        }
    }

    pub async fn release(&self, object: &str, grant: &Grant) {
        let body = serde_json::to_vec(grant).expect("grants serialize");
        self.ask(http::Method::DELETE, object, body.into()).await;
    }

    async fn ask(&self, method: http::Method, object: &str, body: bytes::Bytes) -> Option<(u16, Option<String>, bytes::Bytes)> {
        use http_body_util::BodyExt;
        let req = http::Request::builder().method(method).uri(format!("http://{}{PATH}{object}", crate::ws::PUBLISH_HOST))
            .body(http_body_util::Full::new(body).map_err(|e| match e {}).boxed_unsync()).expect("a valid request");
        let (parts, body) = self.up.send(req).await.ok()?.into_parts();
        let owner = parts.headers.get(OWNER).and_then(|o| o.to_str().ok()).map(String::from);
        Some((parts.status.as_u16(), owner, body.collect().await.ok()?.to_bytes()))
    }
}

/// The supervisor's side of `Worker::ask`, for app `app`: the grant with its
/// session and `until` (200), or the holder (421), or why not (503). A grant
/// goes out only once `claim` has claimed its object for it (see
/// `ship::claim`), and is held from then on.
pub async fn serve<B: http_body::Body<Data = bytes::Bytes>>(
    leases: &Leases,
    app: &str,
    req: http::Request<B>,
    claim: impl AsyncFnOnce(&str, &Grant) -> std::io::Result<()>,
) -> http::Response<bytes::Bytes> {
    use http_body_util::BodyExt;
    let object = req.uri().path().strip_prefix(PATH).unwrap_or_default().to_owned();
    if !crate::table::valid_object(&object) {
        return http::Response::builder().status(400).body(bytes::Bytes::new()).unwrap();
    }
    let key = format!("{app}/{object}");
    let res = http::Response::builder();
    if req.method() == http::Method::DELETE {
        let body = req.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
        if let Ok(grant) = serde_json::from_slice(&body) {
            leases.release(&key, &grant).await;
        }
        return res.status(204).body(bytes::Bytes::new()).unwrap();
    }
    let grant = match leases.acquire(&key).await {
        Ok(grant) => grant,
        Err(Refused::Held(node)) => return res.status(421).header(OWNER, node).body(bytes::Bytes::new()).unwrap(),
        Err(Refused::Unavailable(why)) => return res.status(503).body(why.into()).unwrap(),
    };
    if let Err(e) = claim(&object, &grant).await {
        leases.release(&key, &grant).await;
        return res.status(503).body(format!("claiming {key}: {e}").into()).unwrap();
    }
    // The grant with the `until` of its own session, never another's.
    let Some(until) = leases.until(&grant) else { return res.status(503).body("this node's session ended".into()).unwrap() };
    let body = serde_json::to_vec(&(&grant, grant.session, until)).expect("grants serialize");
    leases.hold(&key, grant);
    res.body(body.into()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_millis(200);

    fn nodes(n: usize) -> (Arc<object_store::memory::InMemory>, Vec<Leases>) {
        let store = Arc::new(object_store::memory::InMemory::new());
        (store.clone(), (0..n).map(|i| Leases::new(store.clone(), &format!("n{i}"), TTL)).collect())
    }

    /// One node at a time holds a lease, each holder at a higher epoch; one
    /// released, or whose node stopped renewing, goes to the next asker.
    #[tokio::test]
    async fn leases_pass_from_node_to_node_at_rising_epochs() {
        let (_, n) = nodes(2);
        assert!(matches!(n[0].acquire("app/o").await, Err(Refused::Unavailable(_))), "no session yet");
        n[0].renew().await;
        n[1].renew().await;
        let first = n[0].acquire("app/o").await.unwrap();
        assert_eq!(first.epoch, 1);
        assert_eq!(n[1].acquire("app/o").await.unwrap_err(), Refused::Held("n0".into()));
        n[0].release("app/o", &first).await;
        let second = n[1].acquire("app/o").await.unwrap();
        assert_eq!(second.epoch, 2);
        // A release of a lease since taken over changes nothing.
        n[0].release("app/o", &first).await;
        assert_eq!(n[0].acquire("app/o").await.unwrap_err(), Refused::Held("n1".into()));
        // n1 stops renewing: n0 is refused until it has seen n1's session
        // unchanged for a whole lease, then takes the object.
        let t0 = Instant::now();
        n[0].renew().await;
        while n[0].acquire("app/o").await.is_err() {
            assert!(t0.elapsed() < TTL * 3, "never taken over");
            n[0].renew().await;
            tokio::time::sleep(TTL / 10).await;
        }
        assert!(t0.elapsed() >= TTL, "taken over after {:?}", t0.elapsed());
        assert_eq!(n[0].acquire("app/o").await.unwrap().epoch, 4, "the takeover was 3");
    }

    /// A holder that keeps renewing keeps its leases however long others
    /// ask; one that can't renew loses them, by its own clock, before any
    /// other node can take them.
    #[tokio::test]
    async fn a_renewing_holder_keeps_its_leases_and_a_lapsed_one_loses_them_first() {
        let (_, n) = nodes(2);
        let (id, _) = n[0].renew().await;
        n[1].renew().await;
        let grant = n[0].acquire("app/o").await.unwrap();
        let t0 = Instant::now();
        while t0.elapsed() < TTL * 3 {
            let (renewed, until) = n[0].renew().await;
            assert!(renewed == id && until > mono_ns());
            n[1].renew().await;
            assert_eq!(n[1].acquire("app/o").await.unwrap_err(), Refused::Held("n0".into()));
            tokio::time::sleep(TTL / 8).await;
        }
        // n0 goes quiet. Its leases lapse at 7/8 of a lease after its last
        // renewal was sent; n1 takes the object no sooner than a whole lease
        // after it last saw that renewal.
        let (_, until) = n[0].renew().await;
        let quiet = mono_ns();
        while n[1].acquire("app/o").await.is_err() {
            n[1].renew().await;
            tokio::time::sleep(TTL / 20).await;
        }
        let taken = mono_ns();
        assert!(until < taken && taken - quiet >= TTL.as_nanos() as u64, "lapsed at {until}, taken at {taken}");
        // n0's session ended: it renews a new one, which its old grant isn't in.
        let (renewed, _) = n[0].renew().await;
        assert!(renewed > id && grant.session == id);
    }

    /// Nodes configured with different lease durations: one with a shorter
    /// lease takes an object over no sooner than the holder's own leases
    /// have lapsed, by the holder's duration, not its own.
    #[tokio::test]
    async fn review_shorter_observer_ttl_cannot_revoke_a_live_holder() {
        let store = Arc::new(object_store::memory::InMemory::new());
        let (a, b) = (Leases::new(store.clone(), "a", TTL * 5), Leases::new(store, "b", TTL / 2));
        let (_, until) = a.renew().await;
        b.renew().await;
        a.acquire("app/o").await.unwrap();
        // A goes quiet; B keeps asking.
        let t0 = Instant::now();
        while b.acquire("app/o").await.is_err() {
            assert!(t0.elapsed() < TTL * 10, "never taken over");
            b.renew().await;
            tokio::time::sleep(TTL / 20).await;
        }
        let taken = mono_ns();
        assert!(until < taken, "B took the object {} ms before A's leases lapsed", (until - taken) / 1_000_000);
    }

    /// A renewal sent before a takeover but landing after it can't extend
    /// the old session: the taker ended it first.
    #[tokio::test]
    async fn a_late_renewal_cannot_outlive_a_takeover() {
        let (store, n) = nodes(2);
        n[0].renew().await;
        n[1].renew().await;
        n[0].acquire("app/o").await.unwrap();
        // The renewal n0 would send, written as it would be.
        let stale = n[0].session.lock().clone();
        n[1].acquire("app/o").await.unwrap_err();
        tokio::time::sleep(TTL).await;
        n[1].renew().await;
        n[1].acquire("app/o").await.unwrap();
        let late = store.put_opts(&Path::from("nodes/n0"),
            serde_json::to_vec(&NodeRecord { session: stale.id, beat: stale.beat + 1, lease_ms: TTL.as_millis() as u64 }).unwrap().into(),
            PutMode::Update(stale.version.unwrap()).into()).await;
        assert!(matches!(late, Err(object_store::Error::Precondition { .. })), "{late:?}");
    }

    /// A store that takes each write of an object's lease record, then
    /// answers it only once `gate` has a permit (a store slow to answer).
    #[derive(Debug)]
    struct Slow {
        store: Arc<object_store::memory::InMemory>,
        landed: tokio::sync::Notify,
        gate: tokio::sync::Semaphore,
    }

    impl std::fmt::Display for Slow {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "Slow")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for Slow {
        async fn put_opts(&self, l: &Path, p: object_store::PutPayload, o: object_store::PutOptions) -> Result<object_store::PutResult> {
            let put = self.store.put_opts(l, p, o).await;
            if l.as_ref().starts_with("objects/") {
                self.landed.notify_one();
                self.gate.acquire().await.unwrap().forget();
            }
            put
        }
        async fn put_multipart_opts(&self, l: &Path, o: object_store::PutMultipartOptions) -> Result<Box<dyn object_store::MultipartUpload>> {
            self.store.put_multipart_opts(l, o).await
        }
        async fn get_opts(&self, l: &Path, o: object_store::GetOptions) -> Result<object_store::GetResult> {
            self.store.get_opts(l, o).await
        }
        fn delete_stream(&self, l: futures::stream::BoxStream<'static, Result<Path>>) -> futures::stream::BoxStream<'static, Result<Path>> {
            self.store.delete_stream(l)
        }
        fn list(&self, p: Option<&Path>) -> futures::stream::BoxStream<'static, Result<object_store::ObjectMeta>> {
            self.store.list(p)
        }
        async fn list_with_delimiter(&self, p: Option<&Path>) -> Result<object_store::ListResult> {
            self.store.list_with_delimiter(p).await
        }
        async fn copy_opts(&self, f: &Path, t: &Path, o: object_store::CopyOptions) -> Result<()> {
            self.store.copy_opts(f, t, o).await
        }
    }

    /// Node A takes a lease, but the store answers late: meanwhile A's
    /// session lapses, B takes the object over (ending A's session) and A
    /// starts a new session. A's answer to its worker must not then make the
    /// grant of the dead session one the worker commits under.
    #[tokio::test(flavor = "multi_thread")]
    async fn review_delayed_acquire_does_not_revive_a_dead_session() {
        let store = Arc::new(object_store::memory::InMemory::new());
        let slow = Arc::new(Slow { store: store.clone(), landed: Default::default(), gate: tokio::sync::Semaphore::new(0) });
        let a = Arc::new(Leases::new(slow.clone(), "a", TTL));
        let b = Leases::new(store, "b", TTL);
        a.renew().await;
        b.renew().await;
        let asked = tokio::spawn({
            let a = a.clone();
            async move {
                let req = http::Request::post(format!("http://nanovm{PATH}o")).body(http_body_util::Empty::<bytes::Bytes>::new());
                serve(&a, "app", req.unwrap(), async |_, _| Ok(())).await
            }
        });
        slow.landed.notified().await;
        let t0 = Instant::now();
        let taken = loop {
            b.renew().await;
            match b.acquire("app/o").await {
                Ok(grant) => break grant,
                Err(_) => assert!(t0.elapsed() < TTL * 5, "B never took the object"),
            }
            tokio::time::sleep(TTL / 10).await;
        };
        // A's worker hears of A's new session, then the late answer.
        let (id, until) = a.renew().await;
        let session = Arc::new(Session::default());
        session.set(id, until);
        slow.gate.add_permits(1);
        let res = asked.await.unwrap();
        // As the worker takes an answer (`Worker::acquire`, `Node::acquire`),
        // and decides a commit (`ObjShared::lease_epoch`).
        let mut obj = crate::object::ObjShared::new("app".into(), "o");
        obj.leases = Some(session.clone());
        if res.status() == 200 {
            let (grant, id, until): (Grant, u64, u64) = serde_json::from_slice(res.body()).unwrap();
            session.set(id, until);
            *obj.lease.get_mut() = Some(grant);
        }
        assert!(obj.lease_epoch().is_err(), "A may commit at epoch {:?} while B holds epoch {}", obj.lease_epoch().ok(), taken.epoch);
        // Nor does any late word on the dead session revive it.
        session.set(id - 1, u64::MAX);
        assert!(!session.holds(&Grant { epoch: 1, session: id - 1, e_tag: None, version: None }));
    }
}
