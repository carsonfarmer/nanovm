//! Leases: which node serves each object, when there are several (W-10).
//!
//! The proxy places each object on a home node, but placement alone can't
//! keep an object to one live instance: nodes stop, and proxies may disagree
//! about the list. So before a node instantiates an object it takes the
//! object's lease, a record in an object store naming the node and an epoch
//! one past the last; a node that doesn't hold the lease refuses the object's
//! requests (421, naming the holder), and the proxy sends them there. A node
//! commits an object's changes only while it holds the lease, and the commit
//! carries the epoch (see `table::Node::lease_epoch`). With no store there is
//! one node, and every lease is its own at epoch 0.
//!
//! Every write is conditional on the version read, so of two nodes racing for
//! a lease one wins. A node renews one record for all its leases, its
//! session, every `lease_ms / 4`; they last until 7/8 of `lease_ms` after it
//! sent the last renewal that succeeded, after which it serves none of their
//! objects and starts a new session. Another node takes an object whose
//! holder's session record it has seen unchanged for `lease_ms`, by its own
//! clock (clocks needn't agree, only tick at about the same rate). It first
//! ends that session with a conditional write, so a renewal still on its way
//! can't land after it and extend the holder's leases.
//!
//! Records: `nodes/<node>` is `{session, beat}` and `objects/<app>/<object>`
//! `{node, session, epoch}`, with no node once released. Deleting a record
//! restarts its epochs: never do.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::time::{Duration, Instant};

use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, UpdateVersion};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Where a worker asks its supervisor for leases: `POST` takes one, `DELETE`
/// gives it up.
pub const PATH: &str = "/_lease/";
/// The node a 421 names as holding the object.
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
    e_tag: Option<String>,
    version: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct NodeRecord {
    session: u64,
    beat: u64,
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

#[derive(Clone, Default)]
struct Session {
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
    session: Mutex<Session>,
    /// Other nodes' session records: the version seen, and since when.
    seen: Mutex<HashMap<String, (Option<UpdateVersion>, Instant)>>,
}

type Result<T> = object_store::Result<T>;

impl Leases {
    pub fn new(store: Arc<dyn ObjectStore>, node: &str, ttl: Duration) -> Leases {
        Leases { store, node: node.into(), ttl, session: Mutex::default(), seen: Mutex::default() }
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
                Ok((rec, version)) => s = Session { id: rec.map_or(1, |r| r.session + 1), beat: 0, version, until: 0 },
                Err(_) => return (s.id, s.until),
            }
        }
        match self.put(&path, &NodeRecord { session: s.id, beat: s.beat + 1 }, s.version.clone()).await {
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
    /// unless another node's live session holds it.
    pub async fn acquire(&self, key: &str) -> std::result::Result<Grant, Refused> {
        let Session { id, until, .. } = self.session.lock().clone();
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
            match self.put(&path, &ObjectRecord { node: self.node.clone(), session: id, epoch }, version).await {
                Ok(v) => return Ok(Grant { epoch, session: id, e_tag: v.e_tag, version: v.version }),
                Err(object_store::Error::Precondition { .. } | object_store::Error::AlreadyExists { .. }) => continue,
                Err(e) => return Err(unavailable(e)),
            }
        }
        Err(Refused::Unavailable(format!("{key}'s lease keeps changing")))
    }

    /// Gives up the lease `grant` took on `key`, unless it has moved on.
    pub async fn release(&self, key: &str, grant: &Grant) {
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
        if rec.is_none_or(|r| r.session != session) {
            return Ok(false);
        }
        {
            let mut seen = self.seen.lock();
            let (seen_version, since) = seen.entry(node.into()).or_insert((None, Instant::now()));
            if *seen_version != version {
                (*seen_version, *since) = (version.clone(), Instant::now());
            }
            if since.elapsed() < self.ttl {
                return Ok(true);
            }
        }
        match self.put(&path, &NodeRecord { session: session + 1, beat: 0 }, version).await {
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

/// A worker's side: its node's session as the supervisor last told it, and
/// the link to ask it for leases.
pub struct Worker {
    id: AtomicU64,
    until: AtomicU64,
    up: crate::egress::Upstream,
}

impl Worker {
    pub fn new(up: crate::egress::Upstream) -> Worker {
        Worker { id: AtomicU64::new(0), until: AtomicU64::new(0), up }
    }

    /// Records the node's session; whether it is a new one.
    pub fn set(&self, id: u64, until: u64) -> bool {
        // The id first: whoever sees the new `until` sees the new id.
        let old = self.id.swap(id, SeqCst);
        self.until.store(until, SeqCst);
        old != id
    }

    /// Whether `grant` is in the node's session and that hasn't lapsed.
    pub fn holds(&self, grant: &Grant) -> bool {
        let until = self.until.load(SeqCst);
        grant.session == self.id.load(SeqCst) && mono_ns() < until
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
/// session and `until` (200), or the holder (421), or why not (503).
pub async fn serve(leases: &Leases, app: &str, req: http::Request<hyper::body::Incoming>) -> http::Response<bytes::Bytes> {
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
    match leases.acquire(&key).await {
        Ok(grant) => {
            let until = leases.session.lock().until;
            res.body(serde_json::to_vec(&(&grant, grant.session, until)).expect("grants serialize").into()).unwrap()
        }
        Err(Refused::Held(node)) => res.status(421).header(OWNER, node).body(bytes::Bytes::new()).unwrap(),
        Err(Refused::Unavailable(why)) => res.status(503).body(why.into()).unwrap(),
    }
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
            serde_json::to_vec(&NodeRecord { session: stale.id, beat: stale.beat + 1 }).unwrap().into(),
            PutMode::Update(stale.version.unwrap()).into()).await;
        assert!(matches!(late, Err(object_store::Error::Precondition { .. })), "{late:?}");
    }
}
