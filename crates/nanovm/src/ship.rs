//! Ships each object's committed versions to a store off the node, and
//! keeps there which of them count: the object's head (W-9, W-90).
//!
//! A version is put once, at `<app>/<object>/<sequence>-<sha256>.tar`: a put
//! never replaces what is there, and the key names the bytes, so writers
//! never clash and a retry is harmless. A version counts only once the
//! object's head, `<app>/<object>/head`, names it. The head moves one
//! version at a time, to the next sequence with the head's version as its
//! parent, by a write conditional on the head read. Restoring follows the
//! head back to the last snapshot, and a commit acknowledged remotely
//! (`remote_ack`) waits for the head to name its version.
//!
//! With leases (W-10), a node moves an object's head only while it holds the
//! object's lease, live, and has claimed the head at that lease's epoch
//! (`claim`, before the object is served there). A claim is a conditional
//! write too, so a node that lost the object may still put a version, but
//! the head it read has moved, and the version never counts. A claim also
//! makes the node's copy of the object the head's, restoring it from the
//! store when it isn't: a node never serves state older than the head. Shipped versions
//! are removed here but the last, which the next names as its parent; what
//! isn't shipped yet stays, within the capture limits, which refuse writes
//! once they are reached (the store down, say), never dropping a version.

use std::io::{self, ErrorKind};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use object_store::{ObjectStore, ObjectStoreExt, PutMode, UpdateVersion};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::CaptureConfig;
use crate::lease::Leases;

pub enum Put {
    Stored,
    /// The same bytes were there already.
    Same,
    /// Other bytes were.
    Conflict,
}

/// The version of a record, to write it again only if it is still that.
pub type Tag = UpdateVersion;

/// Where versions go: keys whose bytes are put once, and records (heads)
/// written only over the version read.
#[async_trait::async_trait]
pub trait Store: Send + Sync {
    /// Stores `bytes` at `key`, unless something is there.
    async fn put(&self, key: &str, bytes: Vec<u8>) -> io::Result<Put>;
    async fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>>;
    /// The record at `key`, and its version.
    async fn read(&self, key: &str) -> io::Result<Option<(Vec<u8>, Tag)>>;
    /// Writes the record at `key` if it is still at `tag` (none: absent):
    /// its new version, or none if it has moved.
    async fn swap(&self, key: &str, bytes: Vec<u8>, tag: Option<&Tag>) -> io::Result<Option<Tag>>;
}

/// The store `cap` names, if any: a directory (`/path` or `file:///path`),
/// or an object store URL as `leases` takes.
pub fn store(cap: &CaptureConfig) -> anyhow::Result<Option<Arc<dyn Store>>> {
    Ok(match cap.store.as_deref() {
        None => None,
        // `object_store`'s own local files can't swap a record.
        Some(dir) if dir.starts_with('/') || dir.starts_with("file:///") => {
            Some(Arc::new(Dir(dir.trim_start_matches("file://").into())))
        }
        Some(url) => Leases::open(url)?.map(|s| Arc::new(Objects(s)) as Arc<dyn Store>),
    })
}

fn hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// A local (or mounted) directory: a put is a hard link, which never
/// replaces a name; a swap renames over a record under its lock file's lock.
pub struct Dir(pub PathBuf);

/// Writes `bytes` (synced if `synced`) to a new file in `dir` that no other
/// write can open: its name is random and it is created exclusively.
/// Returns its path.
fn written(dir: &Path, bytes: &[u8], synced: bool) -> io::Result<PathBuf> {
    use std::io::Write;
    let mut name = [0u8; 8];
    // SAFETY: getentropy writes at most `name.len()` bytes into `name`.
    if unsafe { libc::getentropy(name.as_mut_ptr().cast(), name.len()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let path = dir.join(format!("~new.{:016x}", u64::from_ne_bytes(name)));
    let mut file = std::fs::File::options().write(true).create_new(true).open(&path)?;
    match file.write_all(bytes).and_then(|()| if synced { file.sync_all() } else { Ok(()) }) {
        Ok(()) => Ok(path),
        Err(e) => Err(std::fs::remove_file(&path).err().unwrap_or(e)),
    }
}

/// Syncs `dir`, which a name changed in; and when it was `fresh` (made just
/// now), the directories above it up to `root`, which it may have made too.
fn sync_names(root: &Path, dir: &Path, fresh: bool) -> io::Result<()> {
    let names = if fresh { dir.ancestors().position(|d| d == root).map_or(1, |n| n + 1) } else { 1 };
    dir.ancestors().take(names).try_for_each(|d| std::fs::File::open(d)?.sync_all())
}

/// What a directory's record is at: its digest.
fn tag(bytes: &[u8]) -> Tag {
    UpdateVersion { e_tag: Some(hex(bytes)), version: None }
}

#[async_trait::async_trait]
impl Store for Dir {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> io::Result<Put> {
        let (root, path) = (self.0.clone(), self.0.join(key));
        tokio::task::spawn_blocking(move || {
            let dir = path.parent().expect("keys have a directory");
            let fresh = !dir.exists();
            std::fs::create_dir_all(dir)?;
            let new = written(dir, &bytes, true)?;
            // A link never replaces a name: of puts racing at a key, one
            // stores its bytes, and the others find them.
            let linked = std::fs::hard_link(&new, &path);
            std::fs::remove_file(&new)?;
            match linked {
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    Ok(if std::fs::read(&path)? == bytes { Put::Same } else { Put::Conflict })
                }
                linked => linked.and_then(|()| sync_names(&root, dir, fresh)).map(|()| Put::Stored),
            }
        })
        .await?
    }

    async fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        match tokio::fs::read(self.0.join(key)).await {
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            r => r.map(Some),
        }
    }

    async fn read(&self, key: &str) -> io::Result<Option<(Vec<u8>, Tag)>> {
        Ok(self.get(key).await?.map(|bytes| {
            let at = tag(&bytes);
            (bytes, at)
        }))
    }

    async fn swap(&self, key: &str, bytes: Vec<u8>, at: Option<&Tag>) -> io::Result<Option<Tag>> {
        let (root, path, at) = (self.0.clone(), self.0.join(key), at.cloned());
        tokio::task::spawn_blocking(move || {
            let dir = path.parent().expect("keys have a directory");
            let fresh = !dir.exists();
            std::fs::create_dir_all(dir)?;
            // One swap of a record at a time, in any process.
            let lock = std::fs::File::options().create(true).append(true).open(path.with_extension("lock"))?;
            lock.lock()?;
            let now = match std::fs::read(&path) {
                Err(e) if e.kind() == ErrorKind::NotFound => None,
                now => Some(tag(&now?)),
            };
            if now != at {
                return Ok(None);
            }
            let new = written(dir, &bytes, true)?;
            std::fs::rename(&new, &path)?;
            sync_names(&root, dir, fresh)?;
            Ok(Some(tag(&bytes)))
        })
        .await?
    }
}

/// An object store (S3 and the like): a put is conditional on nothing
/// being there, a swap on the record's ETag.
pub struct Objects(pub Arc<dyn ObjectStore>);

#[async_trait::async_trait]
impl Store for Objects {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> io::Result<Put> {
        match self.0.put_opts(&key.into(), bytes.clone().into(), PutMode::Create.into()).await {
            Ok(_) => Ok(Put::Stored),
            Err(object_store::Error::AlreadyExists { .. } | object_store::Error::Precondition { .. }) => {
                Ok(if self.get(key).await?.is_some_and(|b| b == bytes) { Put::Same } else { Put::Conflict })
            }
            Err(e) => Err(io::Error::other(e)),
        }
    }

    async fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        Ok(self.read(key).await?.map(|(bytes, _)| bytes))
    }

    async fn read(&self, key: &str) -> io::Result<Option<(Vec<u8>, Tag)>> {
        let res = match self.0.get(&key.into()).await {
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            res => res.map_err(io::Error::other)?,
        };
        let tag = UpdateVersion { e_tag: res.meta.e_tag.clone(), version: res.meta.version.clone() };
        Ok(Some((res.bytes().await.map_err(io::Error::other)?.into(), tag)))
    }

    async fn swap(&self, key: &str, bytes: Vec<u8>, at: Option<&Tag>) -> io::Result<Option<Tag>> {
        let mode = at.cloned().map_or(PutMode::Create, PutMode::Update);
        match self.0.put_opts(&key.into(), bytes.into(), mode.into()).await {
            Ok(r) => Ok(Some(UpdateVersion { e_tag: r.e_tag, version: r.version })),
            Err(object_store::Error::AlreadyExists { .. } | object_store::Error::Precondition { .. }) => Ok(None),
            Err(e) => Err(io::Error::other(e)),
        }
    }
}

/// What an object's head says.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Head {
    /// The last version that counts: its sequence, and the SHA-256 its key
    /// names (0 and empty: none yet).
    pub sequence: u64,
    pub digest: String,
    /// The node that claimed the object last, and its lease's epoch then
    /// (empty and 0 with one node): the only one that may move the head.
    pub node: String,
    pub lease: u64,
}

fn version_key(app: &str, object: &str, sequence: u64, digest: &str) -> String {
    format!("{app}/{object}/{sequence:020}-{digest}.tar")
}

fn head_key(app: &str, object: &str) -> String {
    format!("{app}/{object}/head")
}

/// The head of object `object` of app `app`, and its version (none: it has
/// none yet).
pub async fn head(store: &dyn Store, app: &str, object: &str) -> io::Result<(Head, Option<Tag>)> {
    match store.read(&head_key(app, object)).await? {
        None => Ok((Head::default(), None)),
        Some((bytes, tag)) => Ok((serde_json::from_slice(&bytes)?, Some(tag))),
    }
}

/// Where the shipper records the last version the head has named of the
/// object whose versions are in `dir` (`<objects>/~committed/<object>`):
/// `<objects>/~shipped/<object>`.
pub(crate) fn marker(dir: &Path) -> PathBuf {
    let objects = dir.parent().and_then(Path::parent).expect("in ~committed");
    objects.join("~shipped").join(dir.file_name().expect("an object's"))
}

/// Records `sequence` in `marker`, whole: through a file of its own, as
/// claims and the shipper mark objects at once. Unsynced: a marker a crash
/// takes back is caught up from the head (see `ship_object`).
fn mark(marker: &Path, sequence: u64) -> io::Result<()> {
    let dir = marker.parent().expect("in ~shipped");
    std::fs::create_dir_all(dir)?;
    std::fs::rename(written(dir, sequence.to_string().as_bytes(), false)?, marker)
}

/// The last version of the object whose versions are in `dir` that the
/// shipper has seen its head name (0: none).
fn shipped(dir: &Path) -> u64 {
    std::fs::read_to_string(marker(dir)).ok().and_then(|s| s.parse().ok()).unwrap_or(0)
}

/// Waits, two seconds at most (within an instance's post-return deadline),
/// for the shipper to make the version `sequence` of the object whose
/// versions are in `dir` count: for its head to name it, or a later one.
pub(crate) fn stored(dir: &Path, sequence: u64) -> io::Result<()> {
    for _ in 0..1000 {
        if shipped(dir) >= sequence {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(io::Error::new(ErrorKind::TimedOut, format!("version {sequence} is not in the store")))
}

/// The sequences of the versions published in `dir`, in order (none if it
/// isn't there), and whether one is being staged.
fn published(dir: &Path) -> io::Result<(Vec<u64>, bool)> {
    let (mut versions, mut staged) = (vec![], false);
    let entries = match std::fs::read_dir(dir) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok((versions, staged)),
        entries => entries?,
    };
    for entry in entries {
        let name = entry?.file_name();
        let name = name.to_str().unwrap_or_default();
        staged |= name.ends_with(".staged");
        versions.extend(name.strip_suffix(".tar").and_then(|n| n.parse::<u64>().ok()));
    }
    versions.sort();
    Ok((versions, staged))
}

/// The node, and its lease's epoch, under which this node may move the head
/// of object `object` of app `app`: its live lease on the object (with one
/// node, there are no leases: "" and 0); none without one.
fn authority<'a>(leases: Option<&'a Leases>, app: &str, object: &str) -> Option<(&'a str, u64)> {
    let Some(leases) = leases else { return Some(("", 0)) };
    leases.epoch(&format!("{app}/{object}")).map(|epoch| (leases.node(), epoch))
}

/// Ships what the object whose versions are in `dir` hasn't yet, in order,
/// moving its head to each, then removes the shipped ones here but the
/// last. Only under this node's live lease on the object (`authority`), once
/// it has claimed the head with it; and only versions that follow the head,
/// so a node never ships history the head has moved past. `known` is the
/// head as this node last moved it, if it still may be (a move from it
/// fails if it has moved since): no need to read it again. Whether it
/// shipped what it had: not when this node holds no live lease on the
/// object, which isn't a failure (its versions wait until it does).
pub(crate) async fn ship_object(
    store: &dyn Store,
    leases: Option<&Leases>,
    app: &str,
    object: &str,
    dir: &Path,
    known: &mut Option<(Head, Tag)>,
) -> io::Result<bool> {
    let (marker, shipped) = (marker(dir), shipped(dir));
    let (versions, _) = published(dir)?;
    let unshipped: Vec<_> = versions.iter().copied().filter(|&n| n > shipped).collect();
    if !unshipped.is_empty() {
        let Some((node, lease)) = authority(leases, app, object) else { return Ok(false) };
        let (mut head, mut tag) = match known.take().filter(|(head, _)| (head.node.as_str(), head.lease) == (node, lease)) {
            Some((head, tag)) => (head, Some(tag)),
            None => head(store, app, object).await?,
        };
        if (head.node.as_str(), head.lease) != (node, lease) {
            return Err(io::Error::other(format!("{app}/{object}'s head is claimed by {:?} at epoch {}", head.node, head.lease)));
        }
        let at = |n: u64| dir.join(format!("{n:020}.tar"));
        for n in unshipped {
            let astray = || io::Error::other(format!("{app}/{object} version {n} doesn't follow its head, {}", head.sequence));
            if n > head.sequence {
                let bytes = tokio::fs::read(at(n)).await?;
                let (digest, version) = (hex(&bytes), crate::committed::identity(&bytes[..])?);
                if n != head.sequence + 1 || version.parent != head.digest || version.epoch > lease {
                    return Err(astray());
                }
                if let Put::Conflict = store.put(&version_key(app, object, n, &digest), bytes).await? {
                    return Err(io::Error::other(format!("{app}/{object} version {n} isn't what its key names")));
                }
                let next = Head { sequence: n, digest, ..head.clone() };
                let swapped = store.swap(&head_key(app, object), serde_json::to_vec(&next)?, tag.as_ref()).await?;
                tag = Some(swapped.ok_or_else(|| io::Error::other(format!("{app}/{object}'s head moved")))?);
                head = next;
            // One the head has reached was shipped, not marked (a crash, or a
            // power loss taking the marker back): it is the head's history if
            // this copy has the head's version (versions here are a chain).
            } else if crate::committed::digest(&at(head.sequence))? != head.digest {
                return Err(astray());
            }
            mark(&marker, n)?;
        }
        *known = tag.map(|tag| (head, tag));
    }
    for &n in versions.iter().rev().skip(1) {
        match std::fs::remove_file(dir.join(format!("{n:020}.tar"))) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(true)
}

/// How many objects the shipper ships at once, each its versions in order:
/// a version costs a put and a move of its head, a store's round trip each,
/// while many objects commit at once. Each holds one version in memory.
const SHIPPING: usize = 32;

/// What the shipper knows of an object.
#[derive(Default)]
struct Shipping {
    /// Its directory's time when all of it was last shipped.
    seen: Option<std::time::SystemTime>,
    /// Its head as this node last moved it (see `ship_object`).
    head: Option<(Head, Tag)>,
    /// When to try it again, after a failure.
    retry: Option<std::time::Instant>,
}

/// Ships, until the runtime stops, the versions published under `objects`
/// (`<data_dir>/objects`): every app's, every object's, `SHIPPING` objects
/// at once, each in order. A failure is the object's own: logged, and tried
/// again a second later. An object this node holds no live lease on waits,
/// quietly, until it does.
pub async fn run(objects: PathBuf, store: Arc<dyn Store>, leases: Option<Arc<Leases>>) {
    // None while being shipped: it comes back on `done`.
    let mut known: std::collections::HashMap<PathBuf, Option<Shipping>> = Default::default();
    let (back, mut done) = tokio::sync::mpsc::unbounded_channel();
    let slots = Arc::new(tokio::sync::Semaphore::new(SHIPPING));
    loop {
        while let Ok((dir, state)) = done.try_recv() {
            known.insert(dir, Some(state));
        }
        let dirs = std::fs::read_dir(&objects).into_iter().flatten().flatten().flat_map(|app| {
            let committed = app.path().join("~committed");
            let objects = std::fs::read_dir(committed).into_iter().flatten().flatten();
            objects.filter(|o| o.file_type().is_ok_and(|t| t.is_dir())).map(move |o| (app.file_name(), o))
        });
        for (app, object) in dirs.collect::<Vec<_>>() {
            let entry = known.entry(object.path()).or_insert_with(|| Some(Shipping::default()));
            let Some(state) = entry else { continue };
            // Skip an object nothing was published for since its last round:
            // its directory's time is the same, and older than the clock's tick.
            let changed = object.metadata().and_then(|m| m.modified()).ok();
            let old = changed.is_some_and(|t| t.elapsed().is_ok_and(|e| e > Duration::from_millis(100)));
            if (old && state.seen == changed) || state.retry.is_some_and(|t| t > std::time::Instant::now()) {
                continue;
            }
            let mut state = entry.take().expect("not being shipped");
            let (app, id, dir) = (app.to_string_lossy().into_owned(), object.file_name().to_string_lossy().into_owned(), object.path());
            let (store, leases, slots, back) = (store.clone(), leases.clone(), slots.clone(), back.clone());
            tokio::spawn(async move {
                let _slot = slots.acquire().await;
                state.retry = None;
                match ship_object(&*store, leases.as_deref(), &app, &id, &dir, &mut state.head).await {
                    Ok(true) => state.seen = changed,
                    Ok(false) => {}
                    Err(e) => {
                        eprintln!("nanovm: shipping {app}/{id}: {e}");
                        state.retry = Some(std::time::Instant::now() + Duration::from_secs(1));
                    }
                }
                let _ = back.send((dir, state));
            });
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Claims the head of object `object` of app `app` for this node's lease on
/// it, at epoch `lease`, before the object is served here: by a conditional
/// write, after which no node with an earlier lease can move the head. This
/// node's copy of the object (under `objects`, `<data_dir>/objects/<app>`)
/// is then the head's: kept if it is (see `current`), else dropped and
/// restored from the store. So a node never serves state older than the
/// head, nor ships history the head has moved past.
pub async fn claim(store: &dyn Store, node: &str, lease: u64, app: &str, object: &str, objects: &Path) -> io::Result<()> {
    let dir = objects.join("~committed").join(object);
    for _ in 0..4 {
        // This copy as it is before the head is read: the shipper may move
        // the head meanwhile (under the grant it began with), and removes
        // the versions it passed only after, so a head read after names a
        // version listed here, if this copy has it.
        let here = published(&dir)?;
        let (head, tag) = head(store, app, object).await?;
        if head.lease > lease || (head.lease == lease && head.node != node) {
            return Err(io::Error::other(format!("{app}/{object} is claimed at a later epoch, {}", head.lease)));
        }
        let current = match current(objects, object, here, &head, node) {
            // The shipper removed it: the head has moved on.
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            current => current?,
        };
        // Dropped, durably, before the claim, which makes what is here count.
        if !current {
            discard(objects, object)?;
            std::fs::create_dir_all(objects)?;
            sync_fs(objects)?;
        }
        let claimed = Head { node: node.into(), lease, ..head.clone() };
        if store.swap(&head_key(app, object), serde_json::to_vec(&claimed)?, tag.as_ref()).await?.is_none() {
            continue;
        }
        if !current && head.sequence > 0 {
            restore(store, app, object, objects).await?;
        }
        return Ok(());
    }
    Err(io::Error::other(format!("{app}/{object}'s head keeps moving")))
}

/// Whether this node's copy of `object` (under `objects`), with `versions`
/// published (and whether one is being staged), is the head's: it has its
/// volume and the head's version, and versions after it count only if no
/// other node has claimed the object since this one (`node`) did, which
/// another node can't have served. A copy with versions and no volume is
/// one a discard cut short.
fn current(objects: &Path, object: &str, (versions, staged): (Vec<u64>, bool), head: &Head, node: &str) -> io::Result<bool> {
    let at = objects.join("~committed").join(object).join(format!("{:020}.tar", head.sequence));
    let whole = (versions.is_empty() && !staged) || objects.join(object).is_dir();
    let has = head.sequence == 0 || (versions.contains(&head.sequence) && crate::committed::digest(&at)? == head.digest);
    let ours = head.node == node || head.lease == 0;
    Ok(whole && has && (ours || (!staged && versions.last().is_none_or(|&n| n == head.sequence))))
}

/// Drops this node's copy of `object` (under `objects`): its volume first,
/// which a copy with versions has (see `current`), then its journal and
/// marker, and its versions last; so a discard cut short is never taken
/// for a current copy.
fn discard(objects: &Path, object: &str) -> io::Result<()> {
    let [journal, staging, marker, versions] = ["~journal", "~restore", "~shipped", "~committed"].map(|d| objects.join(d).join(object));
    let gone = |done: io::Result<()>| match done {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    };
    for path in [objects.join(object), journal, staging] {
        gone(std::fs::remove_dir_all(path))?;
    }
    gone(std::fs::remove_file(marker))?;
    gone(std::fs::remove_dir_all(versions))
}

/// Rebuilds object `object` of app `app` from what `store` holds, as the
/// volume under `objects` (`<data_dir>/objects/<app>`, where it must not be
/// yet): the version its head names, from the last snapshot before it,
/// which is kept here so history goes on from it. Returns its sequence.
pub async fn restore(store: &dyn Store, app: &str, object: &str, objects: &Path) -> io::Result<u64> {
    let (head, _) = head(store, app, object).await?;
    if head.sequence == 0 {
        return Err(io::Error::new(ErrorKind::NotFound, format!("nothing of {app}/{object} counts in the store")));
    }
    let staging = objects.join("~restore").join(object);
    // What a restore cut short left.
    if let Err(e) = std::fs::remove_dir_all(&staging)
        && e.kind() != ErrorKind::NotFound
    {
        return Err(e);
    }
    std::fs::create_dir_all(&staging)?;
    // Back from the head to the last snapshot, each version the parent the
    // one after it names.
    let (mut n, mut digest) = (head.sequence, head.digest.clone());
    loop {
        let missing = || io::Error::new(ErrorKind::NotFound, format!("{app}/{object} version {n} is not in the store"));
        let bytes = store.get(&version_key(app, object, n, &digest)).await?.ok_or_else(missing)?;
        let version = crate::committed::identity(&bytes[..])?;
        if hex(&bytes) != digest || version.sequence != n {
            return Err(io::Error::new(ErrorKind::InvalidData, format!("{app}/{object} version {n} isn't what its key names")));
        }
        std::fs::write(staging.join(format!("{n:020}.tar")), &bytes)?;
        if version.snapshot || n == 1 {
            break;
        }
        (n, digest) = (n - 1, version.parent);
    }
    let last = crate::committed::restore(&staging, &objects.join(object))?;
    // The volume first, then the version that says it is there.
    sync_fs(objects)?;
    let out = objects.join("~committed").join(object);
    std::fs::create_dir_all(&out)?;
    std::fs::rename(staging.join(format!("{last:020}.tar")), out.join(format!("{last:020}.tar")))?;
    mark(&marker(&out), last)?;
    std::fs::remove_dir_all(staging)?;
    sync_fs(objects)?;
    Ok(last)
}

/// Syncs the filesystem `path` is on.
fn sync_fs(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    tests::sync_point()?;
    let dir = std::fs::File::open(path)?;
    // SAFETY: `dir` is open for the call.
    match unsafe { libc::syncfs(dir.as_raw_fd()) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nanovm-ship-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    thread_local! {
        /// Whether `sync_fs` fails on this thread (a disk that can't sync).
        static FAIL_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    pub(super) fn sync_point() -> io::Result<()> {
        match FAIL_SYNC.get() {
            true => Err(io::Error::from_raw_os_error(libc::EIO)),
            false => Ok(()),
        }
    }

    /// A store each of whose puts and reads can be held once: the next put
    /// waits before it is made, the next read once it is made, after
    /// telling `started`, for a permit from `go`.
    #[derive(Default)]
    struct Stepped {
        store: Option<Objects>,
        put: Step,
        read: Step,
    }

    struct Step {
        armed: std::sync::atomic::AtomicBool,
        started: tokio::sync::Notify,
        go: tokio::sync::Semaphore,
    }

    impl Default for Step {
        fn default() -> Step {
            Step { armed: false.into(), started: Default::default(), go: tokio::sync::Semaphore::new(0) }
        }
    }

    impl Step {
        async fn pass(&self) {
            if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                self.started.notify_one();
                self.go.acquire().await.unwrap().forget();
            }
        }
    }

    #[async_trait::async_trait]
    impl Store for Stepped {
        async fn put(&self, key: &str, bytes: Vec<u8>) -> io::Result<Put> {
            self.put.pass().await;
            self.store.as_ref().unwrap().put(key, bytes).await
        }
        async fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
            self.store.as_ref().unwrap().get(key).await
        }
        async fn read(&self, key: &str) -> io::Result<Option<(Vec<u8>, Tag)>> {
            let read = self.store.as_ref().unwrap().read(key).await;
            self.read.pass().await;
            read
        }
        async fn swap(&self, key: &str, bytes: Vec<u8>, tag: Option<&Tag>) -> io::Result<Option<Tag>> {
            self.store.as_ref().unwrap().swap(key, bytes, tag).await
        }
    }

    /// The shipper ships under the grant it found when it began, which its
    /// object's actor may give up meanwhile, and take again: a claim then
    /// races the shipper on the same node. The shipper moves the head past
    /// the version the claim read it at, and removes that version here; the
    /// claim must still keep the copy, with the version committed (and, with
    /// local acknowledgement, answered) after the shipper listed its own.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_claim_racing_the_shippers_old_grant_keeps_later_versions() {
        let base = tempdir("racing-claim");
        let records: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let store = Arc::new(Stepped { store: Some(Objects(Arc::new(object_store::memory::InMemory::new()))), ..Default::default() });
        let a = Arc::new(Leases::new(records, "a", Duration::from_secs(5)));
        let objects = base.join("a");
        let dir = objects.join("~committed/o");
        let commit = |n: u64, parent: &str, v: &str| {
            std::fs::create_dir_all(objects.join("o")).unwrap();
            std::fs::write(objects.join("o/v"), v).unwrap();
            version(&dir, n, 1, parent, v)
        };
        let first = take(&*store, &a, "o", &objects).await;
        let mut parent = String::new();
        for n in 1..=3 {
            parent = commit(n, &parent, &format!("v{n}"));
        }
        ship_object(&*store, Some(&a), "app", "o", &dir, &mut None).await.unwrap();
        let four = commit(4, &parent, "v4");
        // The shipper lists version 4, reads the head (at 3) and waits to
        // put it.
        store.put.armed.store(true, std::sync::atomic::Ordering::SeqCst);
        let shipper = tokio::spawn({
            let (store, a, dir) = (store.clone(), a.clone(), dir.clone());
            async move { ship_object(&*store, Some(&a), "app", "o", &dir, &mut None).await }
        });
        store.put.started.notified().await;
        commit(5, &four, "v5");
        // The object's actor exits and comes back: its lease is given up
        // and taken again, and claimed. The claim reads the head at 3, then
        // the shipper moves it to 4 and removes version 3 here.
        a.release("app/o", &first).await;
        let again = a.acquire("app/o").await.unwrap();
        store.read.armed.store(true, std::sync::atomic::Ordering::SeqCst);
        let claimed = tokio::spawn({
            let (store, objects) = (store.clone(), objects.clone());
            async move { claim(&*store, "a", again.epoch, "app", "o", &objects).await }
        });
        store.read.started.notified().await;
        store.put.go.add_permits(1);
        shipper.await.unwrap().unwrap();
        store.read.go.add_permits(1);
        claimed.await.unwrap().unwrap();
        let text = std::fs::read_to_string(objects.join("o/v")).map_err(|e| e.to_string());
        assert_eq!((text, published(&dir).unwrap().0), (Ok("v5".into()), vec![4, 5]), "the claim dropped version 5");
        std::fs::remove_dir_all(base).unwrap();
    }

    /// A claim that drops this node's copy (another node claimed the object
    /// since, and this copy has a version the head never named) makes the
    /// drop durable before its claim makes what is here count: else a power
    /// loss could bring the dropped version back, under this node's claim,
    /// and ship it. Where the disk can't sync, nothing is claimed.
    #[tokio::test]
    async fn a_claim_syncs_what_it_dropped_before_claiming() {
        let base = tempdir("durable-drop");
        let store = Objects(Arc::new(object_store::memory::InMemory::new()));
        let objects = base.join("a");
        version(&objects.join("~committed/o"), 1, 1, "", "one");
        std::fs::create_dir_all(objects.join("o")).unwrap();
        std::fs::write(objects.join("o/v"), "one").unwrap();
        let theirs = Head { sequence: 0, digest: String::new(), node: "b".into(), lease: 2 };
        store.swap(&head_key("app", "o"), serde_json::to_vec(&theirs).unwrap(), None).await.unwrap().unwrap();
        FAIL_SYNC.set(true);
        let claimed = claim(&store, "a", 3, "app", "o", &objects).await;
        FAIL_SYNC.set(false);
        assert!(claimed.is_err(), "claimed though the drop can't be made durable");
        assert_eq!(head(&store, "app", "o").await.unwrap().0, theirs);
        claim(&store, "a", 3, "app", "o", &objects).await.unwrap();
        assert_eq!(head(&store, "app", "o").await.unwrap().0.node, "a");
        assert!(!objects.join("o").exists() && published(&objects.join("~committed/o")).unwrap().0.is_empty());
        std::fs::remove_dir_all(base).unwrap();
    }

    /// Both stores keep the first bytes put at a key: the same again is
    /// harmless, other bytes are refused.
    #[tokio::test]
    async fn stores_put_once() {
        let memory = Objects(Arc::new(object_store::memory::InMemory::new()));
        let stores: [Box<dyn Store>; 2] = [Box::new(Dir(tempdir("put"))), Box::new(memory)];
        for store in stores {
            assert!(store.get("a/o/1").await.unwrap().is_none());
            assert!(matches!(store.put("a/o/1", b"one".to_vec()).await.unwrap(), Put::Stored));
            assert!(matches!(store.put("a/o/1", b"one".to_vec()).await.unwrap(), Put::Same));
            assert!(matches!(store.put("a/o/1", b"other".to_vec()).await.unwrap(), Put::Conflict));
            assert_eq!(store.get("a/o/1").await.unwrap().unwrap(), b"one");
        }
    }

    /// Puts racing at one key (two shippers of one object, say) each end
    /// stored, the same, or refused, and the bytes a put stored are the ones
    /// that stay there: no put changes another's.
    #[tokio::test(flavor = "multi_thread")]
    async fn review_concurrent_directory_put_keeps_acknowledged_bytes() {
        let base = tempdir("racing");
        let store = Arc::new(Dir(base.clone()));
        for round in 0..40 {
            let key = format!("a/o/{round:020}.tar");
            let puts: Vec<_> = (0..8u8)
                .map(|i| {
                    let (store, key) = (store.clone(), key.clone());
                    tokio::spawn(async move { (store.put(&key, vec![i; 256 << 10]).await, i) })
                })
                .collect();
            let mut stored = vec![];
            for put in puts {
                match put.await.unwrap() {
                    (Ok(Put::Stored), i) => stored.push(i),
                    (Ok(Put::Conflict), _) => {}
                    (Ok(Put::Same), i) => panic!("round {round}: put {i} found its own bytes"),
                    (Err(e), i) => panic!("round {round}: put {i} failed: {e}"),
                }
            }
            assert_eq!(stored.len(), 1, "round {round}: stored by {stored:?}");
            let kept = store.get(&key).await.unwrap().unwrap();
            assert!(kept == vec![stored[0]; 256 << 10], "round {round}: put {} stored, the key holds {}'s bytes", stored[0], kept[0]);
        }
        let left: Vec<_> = std::fs::read_dir(base.join("a/o")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left.len(), 40, "no temporary files are left: {left:?}");
        std::fs::remove_dir_all(base).unwrap();
    }

    /// Writes version `n` of the object whose versions are in `dir`, committed
    /// at `epoch` after the version with digest `parent`: a snapshot whose
    /// volume holds `v`, saying `text`. Returns its digest.
    fn version(dir: &Path, n: u64, epoch: u64, parent: &str, text: &str) -> String {
        let object = dir.file_name().unwrap().to_str().unwrap();
        let id = serde_json::json!({
            "format": 1, "app": "app", "object": object, "sequence": n, "epoch": epoch, "parent": parent, "snapshot": true,
        });
        let mut tar = tar::Builder::new(vec![]);
        for (path, bytes) in [("nanovm.json", id.to_string().into_bytes()), ("data/v", text.as_bytes().to_vec())] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o600);
            tar.append_data(&mut header, path, &bytes[..]).unwrap();
        }
        let bytes = tar.into_inner().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        // Whole, as a commit publishes it: a shipper may be listing.
        std::fs::write(dir.join("new"), &bytes).unwrap();
        std::fs::rename(dir.join("new"), dir.join(format!("{n:020}.tar"))).unwrap();
        hex(&bytes)
    }

    /// What object `object` of `app` holds, restored from `store` on a fresh
    /// root: the text of its version, or why not.
    async fn restored(store: &dyn Store, object: &str, root: &Path) -> Result<String, String> {
        let _ = std::fs::remove_dir_all(root);
        std::fs::create_dir_all(root).unwrap();
        restore(store, "app", object, root).await.map_err(|e| e.to_string())?;
        Ok(std::fs::read_to_string(root.join(object).join("v")).unwrap())
    }

    /// Takes `object`'s lease for `leases`' node, claims the head for it with
    /// its copy of the object under `objects`, and holds it, as the
    /// supervisor does before a worker serves the object.
    async fn take(store: &dyn Store, leases: &Leases, object: &str, objects: &Path) -> crate::lease::Grant {
        let key = format!("app/{object}");
        let t0 = std::time::Instant::now();
        loop {
            leases.renew().await;
            if let Ok(grant) = leases.acquire(&key).await {
                claim(store, leases.node(), grant.epoch, "app", object, objects).await.unwrap();
                leases.hold(&key, grant.clone());
                return grant;
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "{} never took {object}", leases.node());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Records swap only over the version read, in both stores.
    #[tokio::test]
    async fn records_swap_only_over_what_was_read() {
        let memory = Objects(Arc::new(object_store::memory::InMemory::new()));
        let stores: [Box<dyn Store>; 2] = [Box::new(Dir(tempdir("swap"))), Box::new(memory)];
        for store in stores {
            assert!(store.read("a/o/head").await.unwrap().is_none());
            let one = store.swap("a/o/head", b"1".to_vec(), None).await.unwrap().unwrap();
            assert!(store.swap("a/o/head", b"x".to_vec(), None).await.unwrap().is_none(), "made already");
            let two = store.swap("a/o/head", b"2".to_vec(), Some(&one)).await.unwrap().unwrap();
            assert!(store.swap("a/o/head", b"x".to_vec(), Some(&one)).await.unwrap().is_none(), "moved since");
            assert_eq!(store.read("a/o/head").await.unwrap().unwrap(), (b"2".to_vec(), two));
        }
    }

    /// With one node, the shipper moves the head to each version in order,
    /// then keeps only the last here; a fresh root restores the head's. It
    /// stops at a version that doesn't follow the head (another writer's).
    #[tokio::test]
    async fn the_shipper_follows_the_head_and_keeps_the_last() {
        let base = tempdir("shipper");
        let (store, dir) = (Dir(base.join("store")), base.join("objects/app/~committed/o"));
        let local = || published(&dir).unwrap().0;
        let mut parent = String::new();
        for n in 1..=3 {
            parent = version(&dir, n, 0, &parent, &format!("v{n}"));
        }
        ship_object(&store, None, "app", "o", &dir, &mut None).await.unwrap();
        let shipped = Head { sequence: 3, digest: parent.clone(), node: "".into(), lease: 0 };
        assert_eq!(head(&store, "app", "o").await.unwrap().0, shipped);
        assert_eq!((local(), std::fs::read_to_string(marker(&dir)).unwrap()), (vec![3], "3".into()));
        assert_eq!(restored(&store, "o", &base.join("fresh")).await.unwrap(), "v3");
        // Another writer moves the head on.
        let (_, tag) = head(&store, "app", "o").await.unwrap();
        let theirs = Head { sequence: 4, digest: "0".repeat(64), ..shipped };
        store.swap("app/o/head", serde_json::to_vec(&theirs).unwrap(), tag.as_ref()).await.unwrap().unwrap();
        version(&dir, 4, 0, &parent, "v4");
        assert!(ship_object(&store, None, "app", "o", &dir, &mut None).await.is_err());
        assert_eq!((local(), std::fs::read_to_string(marker(&dir)).unwrap()), (vec![3, 4], "3".into()));
        assert_eq!(head(&store, "app", "o").await.unwrap().0, theirs);
        std::fs::remove_dir_all(base).unwrap();
    }

    /// A store whose next put waits, once `started` has been told, for a
    /// permit from `gate`.
    struct Gated {
        store: Objects,
        armed: std::sync::atomic::AtomicBool,
        started: tokio::sync::Notify,
        gate: tokio::sync::Semaphore,
    }

    #[async_trait::async_trait]
    impl Store for Gated {
        async fn put(&self, key: &str, bytes: Vec<u8>) -> io::Result<Put> {
            if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                self.started.notify_one();
                self.gate.acquire().await.unwrap().forget();
            }
            self.store.put(key, bytes).await
        }
        async fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
            self.store.get(key).await
        }
        async fn read(&self, key: &str) -> io::Result<Option<(Vec<u8>, Tag)>> {
            self.store.read(key).await
        }
        async fn swap(&self, key: &str, bytes: Vec<u8>, tag: Option<&Tag>) -> io::Result<Option<Tag>> {
            self.store.swap(key, bytes, tag).await
        }
    }

    /// Node A uploads a version; before the upload lands, A's lease runs out
    /// and B takes the object over. A's upload lands, but never counts: A's
    /// shipping fails, marks nothing, and B's history is the object's.
    #[tokio::test(flavor = "multi_thread")]
    async fn review_takeover_during_upload_fences_the_old_writer() {
        let base = tempdir("fence");
        let records: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let store = Gated {
            store: Objects(Arc::new(object_store::memory::InMemory::new())),
            armed: true.into(),
            started: Default::default(),
            gate: tokio::sync::Semaphore::new(0),
        };
        let (a, b) = (Leases::new(records.clone(), "a", Duration::from_millis(200)), Leases::new(records, "b", Duration::from_secs(5)));
        let (a_objects, b_objects) = (base.join("a"), base.join("b"));
        let (a_dir, b_dir) = (a_objects.join("~committed/o"), b_objects.join("~committed/o"));
        let ga = take(&store, &a, "o", &a_objects).await;
        version(&a_dir, 1, ga.epoch, "", "A's");
        let mut known = None;
        let (shipped, gb) = tokio::join!(ship_object(&store, Some(&a), "app", "o", &a_dir, &mut known), async {
            store.started.notified().await;
            let gb = take(&store, &b, "o", &b_objects).await;
            store.gate.add_permits(1);
            gb
        });
        assert!(shipped.is_err(), "A's version counts though B took the object at epoch {}", gb.epoch);
        assert!(!marker(&a_dir).exists(), "A marked its version shipped");
        // B serves the object, and its history is the object's.
        let digest = version(&b_dir, 1, gb.epoch, "", "B's");
        ship_object(&store, Some(&b), "app", "o", &b_dir, &mut None).await.unwrap();
        let ours = Head { sequence: 1, digest, node: "b".into(), lease: gb.epoch };
        assert_eq!(head(&store, "app", "o").await.unwrap().0, ours);
        assert_eq!(restored(&store, "o", &base.join("fresh")).await.unwrap(), "B's");
        std::fs::remove_dir_all(base).unwrap();
    }

    /// A claim keeps a node's copy of an object while no other node has
    /// claimed the object since this one did (its versions after the head
    /// then ship); once another has, the copy goes, with any versions after
    /// the head, and the head's is restored.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_claim_keeps_only_history_no_other_node_has_moved_past() {
        let base = tempdir("claims");
        let records: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let store = Objects(Arc::new(object_store::memory::InMemory::new()));
        let (a, b) = (Leases::new(records.clone(), "a", Duration::from_millis(200)), Leases::new(records, "b", Duration::from_secs(5)));
        let (a_objects, b_objects) = (base.join("a"), base.join("b"));
        let dir = a_objects.join("~committed/o");
        let text = |objects: &Path| std::fs::read_to_string(objects.join("o/v")).ok();
        // A version and the volume as it leaves it.
        let commit = |n: u64, epoch: u64, parent: &str, v: &str| {
            std::fs::create_dir_all(a_objects.join("o")).unwrap();
            std::fs::write(a_objects.join("o/v"), v).unwrap();
            version(&dir, n, epoch, parent, v)
        };
        // A ships version 1, commits 2, and lets the object go.
        let first = take(&store, &a, "o", &a_objects).await;
        let one = commit(1, first.epoch, "", "one");
        ship_object(&store, Some(&a), "app", "o", &dir, &mut None).await.unwrap();
        let two = commit(2, first.epoch, &one, "two");
        a.release("app/o", &first).await;
        // Taken again with no other node between: its copy stays, and
        // version 2 ships under the new lease.
        let again = take(&store, &a, "o", &a_objects).await;
        assert_eq!(text(&a_objects).as_deref(), Some("two"));
        ship_object(&store, Some(&a), "app", "o", &dir, &mut None).await.unwrap();
        assert_eq!(head(&store, "app", "o").await.unwrap().0.sequence, 2);
        // A commits 3, but B takes the object before A ships it: B restores 2.
        commit(3, again.epoch, &two, "three");
        let theirs = take(&store, &b, "o", &b_objects).await;
        assert_eq!(text(&b_objects).as_deref(), Some("two"));
        b.release("app/o", &theirs).await;
        // A takes it back: its version 3 never was the object's, and goes.
        take(&store, &a, "o", &a_objects).await;
        assert_eq!((text(&a_objects).as_deref(), published(&dir).unwrap().0), (Some("two"), vec![2]));
        ship_object(&store, Some(&a), "app", "o", &dir, &mut None).await.unwrap();
        assert_eq!(head(&store, "app", "o").await.unwrap().0.sequence, 2);
        std::fs::remove_dir_all(base).unwrap();
    }

    /// A node taking over many objects at once (another node stopped)
    /// claims them concurrently, each claim restoring its object and marking
    /// it shipped up to its head. Each object's marker must name its own
    /// head: a marker past it acknowledges (`stored`) and skips shipping
    /// versions the store never got.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn review_concurrent_claims_mark_each_object_its_own_head() {
        let base = tempdir("marks");
        let store = Arc::new(Objects(Arc::new(object_store::memory::InMemory::new())));
        const OBJECTS: u64 = 16;
        // Object `o{i}`'s head names its version at sequence `1 + 7i`, claimed by node a.
        let sequence = |i: u64| 1 + 7 * i;
        for i in 0..OBJECTS {
            let (object, n) = (format!("o{i}"), sequence(i));
            let dir = base.join("a/~committed").join(&object);
            let digest = version(&dir, n, 1, "", &format!("v{i}"));
            let bytes = std::fs::read(dir.join(format!("{n:020}.tar"))).unwrap();
            store.put(&version_key("app", &object, n, &digest), bytes).await.unwrap();
            let head = Head { sequence: n, digest, node: "a".into(), lease: 1 };
            store.swap(&head_key("app", &object), serde_json::to_vec(&head).unwrap(), None).await.unwrap().unwrap();
        }
        let mut wrong = vec![];
        for round in 0..20u64 {
            // Node b, on a disk of its own, takes every object at once.
            let objects = base.join(format!("b{round}"));
            let claims: Vec<_> = (0..OBJECTS)
                .map(|i| {
                    let (store, objects) = (store.clone(), objects.clone());
                    tokio::spawn(async move { claim(&*store, "b", 2 + round, "app", &format!("o{i}"), &objects).await })
                })
                .collect();
            for (i, c) in claims.into_iter().enumerate() {
                let i = i as u64;
                if let Err(e) = c.await.unwrap() {
                    wrong.push(format!("round {round}: claiming o{i} failed: {e}"));
                    continue;
                }
                let dir = objects.join("~committed").join(format!("o{i}"));
                let marked = std::fs::read_to_string(marker(&dir)).unwrap_or_default();
                if marked != sequence(i).to_string() {
                    let acked = stored(&dir, sequence(i) + 1).is_ok();
                    wrong.push(format!("round {round}: o{i}'s head is {} but its marker says {marked:?} (version {} acknowledged: {acked})",
                        sequence(i), sequence(i) + 1));
                }
            }
        }
        assert!(wrong.is_empty(), "{} of {} claims went wrong:\n{}", wrong.len(), 20 * OBJECTS, wrong.join("\n"));
        std::fs::remove_dir_all(base).unwrap();
    }

    /// A claim that must drop a node's copy (another node claimed the object
    /// since, and this copy has a version the head never named) is cut short
    /// by a crash: the volume is gone, and removing the versions got as far
    /// as the later one (`remove_dir_all` goes in directory order, here the
    /// newest first). The next claim must still restore the head's state,
    /// never serve the object with no volume.
    #[tokio::test(flavor = "multi_thread")]
    async fn review_a_discard_cut_short_is_not_taken_for_a_current_copy() {
        let base = tempdir("cut-short");
        let store = Objects(Arc::new(object_store::memory::InMemory::new()));
        let objects = base.join("a");
        let dir = objects.join("~committed/o");
        let text = || std::fs::read_to_string(objects.join("o/v")).map_err(|e| e.to_string());
        // A shipped version 1, committed 2 (unshipped), then B took the
        // object over at epoch 2 and claimed its head.
        let one = version(&dir, 1, 1, "", "one");
        store.put(&version_key("app", "o", 1, &one), std::fs::read(dir.join(format!("{:020}.tar", 1))).unwrap()).await.unwrap();
        let head = Head { sequence: 1, digest: one.clone(), node: "b".into(), lease: 2 };
        store.swap(&head_key("app", "o"), serde_json::to_vec(&head).unwrap(), None).await.unwrap().unwrap();
        version(&dir, 2, 1, &one, "two");
        mark(&marker(&dir), 1).unwrap();
        std::fs::create_dir_all(objects.join("o")).unwrap();
        std::fs::write(objects.join("o/v"), "two").unwrap();
        // The order discard removes things in, as `remove_dir_all` here does.
        let order: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(order[0], format!("{:020}.tar", 2)[..], "this filesystem lists the newest first");
        // A's claim at epoch 3 is cut short: the volume went, then version 2.
        std::fs::remove_dir_all(objects.join("o")).unwrap();
        std::fs::remove_file(dir.join(format!("{:020}.tar", 2))).unwrap();
        // A claims again, at epoch 4.
        claim(&store, "a", 4, "app", "o", &objects).await.unwrap();
        assert_eq!(text(), Ok("one".into()), "A serves the object with no volume, its versions say version 1");
        std::fs::remove_dir_all(base).unwrap();
    }

    /// A store each of whose calls takes `delay` (an object store's round
    /// trip).
    struct Latent {
        store: Objects,
        delay: Duration,
        /// Records read so far.
        reads: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Store for Latent {
        async fn put(&self, key: &str, bytes: Vec<u8>) -> io::Result<Put> {
            tokio::time::sleep(self.delay).await;
            self.store.put(key, bytes).await
        }
        async fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
            tokio::time::sleep(self.delay).await;
            self.store.get(key).await
        }
        async fn read(&self, key: &str) -> io::Result<Option<(Vec<u8>, Tag)>> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.store.read(key).await
        }
        async fn swap(&self, key: &str, bytes: Vec<u8>, tag: Option<&Tag>) -> io::Result<Option<Tag>> {
            tokio::time::sleep(self.delay).await;
            self.store.swap(key, bytes, tag).await
        }
    }

    /// With remote acknowledgement (the default with leases), each commit
    /// waits up to two seconds (`stored`) for the shipper to make its
    /// version count. Objects committing at once on one node, with a store
    /// answering in 25 ms, must each be acknowledged in that time: else
    /// the write is answered 500 (commit-failed) though it stands.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn review_remote_acks_keep_up_with_objects_committing_at_once() {
        let base = tempdir("acks");
        let objects = base.join("objects");
        const OBJECTS: usize = 40;
        let dirs: Vec<_> = (0..OBJECTS).map(|i| objects.join(format!("app/~committed/o{i}"))).collect();
        for dir in &dirs {
            version(dir, 1, 0, "", "v");
        }
        let store = Arc::new(Latent { store: Objects(Arc::new(object_store::memory::InMemory::new())), delay: Duration::from_millis(25), reads: Default::default() });
        let shipper = tokio::spawn(run(objects, store, None));
        // Each commit's wait, as `Journal::commit` makes it, all at once.
        let waits: Vec<_> = dirs.iter().cloned().map(|dir| tokio::task::spawn_blocking(move || stored(&dir, 1))).collect();
        let mut late = 0;
        for w in waits {
            late += w.await.unwrap().is_err() as usize;
        }
        shipper.abort();
        assert_eq!(late, 0, "{late} of {OBJECTS} commits at once were not acknowledged within the 2 s wait");
        std::fs::remove_dir_all(base).unwrap();
    }

    /// A failure shipping one object (here the store refuses its puts with
    /// PermissionDenied, as a directory store does on EACCES) is that
    /// object's: logged and tried again a second later, never taken for
    /// "this node holds no live lease" (waited out quietly, tried again at
    /// once); and the other objects ship meanwhile.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failure_shipping_one_object_is_its_own_and_not_taken_for_no_lease() {
        struct Denying(Objects, std::sync::atomic::AtomicUsize);
        #[async_trait::async_trait]
        impl Store for Denying {
            async fn put(&self, key: &str, bytes: Vec<u8>) -> io::Result<Put> {
                if key.starts_with("app/bad/") {
                    self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    return Err(ErrorKind::PermissionDenied.into());
                }
                self.0.put(key, bytes).await
            }
            async fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
                self.0.get(key).await
            }
            async fn read(&self, key: &str) -> io::Result<Option<(Vec<u8>, Tag)>> {
                self.0.read(key).await
            }
            async fn swap(&self, key: &str, bytes: Vec<u8>, tag: Option<&Tag>) -> io::Result<Option<Tag>> {
                self.0.swap(key, bytes, tag).await
            }
        }
        let base = tempdir("denied");
        let objects = base.join("objects");
        let dir = |o: &str| objects.join("app/~committed").join(o);
        version(&dir("bad"), 1, 0, "", "v");
        let store = Arc::new(Denying(Objects(Arc::new(object_store::memory::InMemory::new())), Default::default()));
        let shipper = tokio::spawn(run(objects.clone(), store.clone(), None));
        let tries = || store.1.load(std::sync::atomic::Ordering::SeqCst);
        let t0 = std::time::Instant::now();
        while tries() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Another object commits meanwhile, and ships.
        version(&dir("good"), 1, 0, "", "v");
        let good = dir("good");
        tokio::task::spawn_blocking(move || stored(&good, 1)).await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(500).saturating_sub(t0.elapsed())).await;
        assert_eq!(tries(), 1, "tried again at once, as an object with no lease");
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert!(tries() >= 2, "never tried again");
        shipper.abort();
        std::fs::remove_dir_all(base).unwrap();
    }

    /// A marker that a power loss took back (the head names version 3, the
    /// marker 1, and versions 2 and 3 are still here, their removal lost
    /// too) is caught up: versions up to the head's are in its history (a
    /// claim made this copy the head's), so the shipper marks them and goes
    /// on, rather than stopping at a version that "doesn't follow its head"
    /// for good (and, acknowledging remotely, serving the object nothing).
    #[tokio::test]
    async fn the_shipper_catches_up_a_marker_left_behind() {
        let base = tempdir("behind");
        let (store, dir) = (Objects(Arc::new(object_store::memory::InMemory::new())), base.join("objects/app/~committed/o"));
        let mut parents = vec![String::new()];
        for n in 1..=3 {
            parents.push(version(&dir, n, 0, &parents[n as usize - 1], &format!("v{n}")));
        }
        ship_object(&store, None, "app", "o", &dir, &mut None).await.unwrap();
        for n in 2..=3 {
            version(&dir, n, 0, &parents[n as usize - 1], &format!("v{n}"));
        }
        mark(&marker(&dir), 1).unwrap();
        version(&dir, 4, 0, &parents[3], "v4");
        ship_object(&store, None, "app", "o", &dir, &mut None).await.unwrap();
        assert_eq!(head(&store, "app", "o").await.unwrap().0.sequence, 4);
        assert_eq!((published(&dir).unwrap().0, std::fs::read_to_string(marker(&dir)).unwrap()), (vec![4], "4".into()));
        std::fs::remove_dir_all(base).unwrap();
    }

    /// Acknowledged commits per second, and the shipper's CPU per
    /// acknowledgement, with a store answering each call in 25 ms: each of
    /// `n` objects commits a version and waits (`stored`, as a commit with
    /// `remote_ack` does) for it to count, then commits the next, for 10 s.
    /// `cargo test --lib measure_remote_acks -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn measure_remote_acks() {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        // CPU time of this process's threads named `shipper`, in µs.
        let cpu = || -> u64 {
            let ticks: u64 = std::fs::read_dir("/proc/self/task").unwrap().flatten().filter_map(|t| {
                let stat = std::fs::read_to_string(t.path().join("stat")).ok()?;
                let (name, rest) = stat.split_once(" (")?.1.rsplit_once(") ")?;
                let fields: Vec<&str> = rest.split(' ').collect();
                (name == "shipper").then(|| fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap())
            }).sum();
            ticks * 1_000_000 / unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64
        };
        for n in [1, 40, 200] {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).thread_name("shipper").enable_all().build().unwrap();
            let base = tempdir(&format!("measure-{n}"));
            let store = Arc::new(Latent { store: Objects(Arc::new(object_store::memory::InMemory::new())), delay: Duration::from_millis(25), reads: Default::default() });
            rt.spawn(run(base.join("objects"), store, None));
            let (acked, late, cpu0, t0) = (AtomicU64::new(0), AtomicU64::new(0), cpu(), std::time::Instant::now());
            std::thread::scope(|s| {
                for i in 0..n {
                    let (dir, acked, late) = (base.join(format!("objects/app/~committed/o{i}")), &acked, &late);
                    s.spawn(move || {
                        let (mut parent, mut seq) = (String::new(), 0);
                        while t0.elapsed() < Duration::from_secs(10) {
                            seq += 1;
                            parent = version(&dir, seq, 0, &parent, "v");
                            match stored(&dir, seq) {
                                Ok(()) => acked.fetch_add(1, Relaxed),
                                Err(_) => late.fetch_add(1, Relaxed),
                            };
                        }
                    });
                }
            });
            let (secs, used) = (t0.elapsed().as_secs_f64(), cpu() - cpu0);
            let (acked, late) = (acked.load(Relaxed), late.load(Relaxed));
            println!("objects {n}: {:.1} acknowledged commits/s, {late} not acknowledged within 2 s, shipper CPU {} µs per acknowledgement",
                acked as f64 / secs, used / acked.max(1));
            drop(rt);
            std::fs::remove_dir_all(base).unwrap();
        }
    }

    /// The shipper reads an object's head once, then moves it on from what
    /// it last wrote: a version costs a put and a move of the head, two round
    /// trips, not three. A head moved meanwhile (here written again, as a
    /// claim would) fails the move from the stale one, and is read again.
    #[tokio::test]
    async fn the_shipper_reads_a_head_once_then_moves_it_from_what_it_wrote() {
        let base = tempdir("known");
        let store = Latent { store: Objects(Arc::new(object_store::memory::InMemory::new())), delay: Duration::ZERO, reads: Default::default() };
        let dir = base.join("objects/app/~committed/o");
        let reads = || store.reads.load(std::sync::atomic::Ordering::SeqCst);
        let (mut known, mut parent) = (None, String::new());
        for n in 1..=5 {
            parent = version(&dir, n, 0, &parent, "v");
            assert!(ship_object(&store, None, "app", "o", &dir, &mut known).await.unwrap());
        }
        assert_eq!(reads(), 1, "the head was read for every version");
        let (now, tag) = head(&store, "app", "o").await.unwrap();
        store.swap(&head_key("app", "o"), serde_json::to_vec(&now).unwrap(), tag.as_ref()).await.unwrap().unwrap();
        version(&dir, 6, 0, &parent, "v");
        assert!(ship_object(&store, None, "app", "o", &dir, &mut known).await.is_err(), "moved from a stale head");
        assert!(ship_object(&store, None, "app", "o", &dir, &mut known).await.unwrap());
        assert_eq!(reads(), 3, "the head wasn't read again");
        assert_eq!(head(&store, "app", "o").await.unwrap().0.sequence, 6);
        std::fs::remove_dir_all(base).unwrap();
    }

    /// With remote acknowledgement, a commit whose version doesn't count in
    /// the store in time is answered 500 but stands on its node, which goes
    /// on serving it. If that node then stops before shipping it, the node
    /// taking the object over must not serve older state than the old node
    /// served (README: "serves no older state").
    #[tokio::test(flavor = "multi_thread")]
    async fn review_a_takeover_never_serves_older_state_than_was_served() {
        use crate::journal::Journal;
        use std::os::unix::fs::FileExt;
        let base = tempdir("older");
        let records: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let store = Objects(Arc::new(object_store::memory::InMemory::new()));
        let (a, b) = (Leases::new(records.clone(), "a", Duration::from_millis(500)), Leases::new(records, "b", Duration::from_secs(5)));
        let (a_objects, b_objects) = (base.join("a"), base.join("b"));
        let ga = take(&store, &a, "o", &a_objects).await;
        let (id, until) = a.renew().await;
        let session = Arc::new(crate::lease::Session::default());
        session.set(id, until);
        let mut obj = crate::object::ObjShared::new("app".into(), "o");
        (obj.leases, *obj.lease.get_mut()) = (Some(session), Some(ga));
        let obj = Arc::new(obj);
        let volume = a_objects.join("o");
        std::fs::create_dir_all(&volume).unwrap();
        std::fs::write(volume.join("v"), "zero").unwrap();
        let volume = volume.canonicalize().unwrap();
        // Writes `text` over `v` and commits, as a request does.
        let commit = |remote_ack: bool, text: &str| {
            let cap = CaptureConfig { remote_ack, store: Some("/unused".into()), ..Default::default() };
            let mut j = Journal::open(&volume, 1 << 30, obj.clone(), Some(&cap)).unwrap();
            let file = std::fs::File::options().write(true).open(volume.join("v")).unwrap();
            j.writing(&file, 0, text.len() as u64).unwrap();
            file.write_all_at(text.as_bytes(), 0).unwrap();
            j.commit()
        };
        // "zero" counts in the store.
        commit(false, "zero").unwrap();
        ship_object(&store, Some(&a), "app", "o", &a_objects.join("~committed/o"), &mut None).await.unwrap();
        // "one!" is committed, but the store is slow: answered 500.
        assert!(commit(true, "one!").is_err());
        // A's next instance recovers the journal and serves the object.
        drop(Journal::open(&volume, 1 << 30, obj.clone(), Some(&CaptureConfig::default())).unwrap());
        let served = std::fs::read_to_string(volume.join("v")).unwrap();
        assert_eq!(served, "one!", "A serves what it answered 500 for");
        // A stops; B takes the object over.
        tokio::time::sleep(Duration::from_millis(500)).await;
        take(&store, &b, "o", &b_objects).await;
        let theirs = std::fs::read_to_string(b_objects.join("o/v")).unwrap();
        assert_eq!(theirs, served, "B serves older state than A served");
        std::fs::remove_dir_all(base).unwrap();
    }

    /// A node ships an object's versions only under its live lease on the
    /// object, at the epoch it claimed the head with: not with no lease, nor
    /// a released, lapsed, or since-replaced one, though the lease record may
    /// still name the node.
    #[tokio::test(flavor = "multi_thread")]
    async fn review_shipping_requires_a_live_matching_epoch_not_just_a_node_name() {
        let base = tempdir("authority");
        let records: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let store = Objects(Arc::new(object_store::memory::InMemory::new()));
        let ttl = Duration::from_millis(200);
        let (a, b) = (Leases::new(records.clone(), "a", ttl), Leases::new(records, "b", ttl * 25));
        let objects = base.join("a");
        // A has a version of `object`, committed at `epoch`; it must not count.
        let mut shipped = vec![];
        let mut refused = async |object: &str, epoch: u64| {
            let dir = objects.join("~committed").join(object);
            version(&dir, 1, epoch, "", "late");
            if ship_object(&store, Some(&a), "app", object, &dir, &mut None).await.is_ok_and(|shipped| shipped) || head(&store, "app", object).await.unwrap().0.sequence > 0 {
                shipped.push(object.to_owned());
            }
        };
        // No lease on the object.
        a.renew().await;
        refused("missing", 1).await;
        // A released lease (the record names no node, at the version's epoch).
        let grant = take(&store, &a, "released", &objects).await;
        a.release("app/released", &grant).await;
        refused("released", grant.epoch).await;
        // A lapsed lease (the record still names A, at the version's epoch).
        let grant = take(&store, &a, "lapsed", &objects).await;
        tokio::time::sleep(ttl).await;
        refused("lapsed", grant.epoch).await;
        // A lease taken again, after B held the object: the record names A,
        // at a later epoch than the version's.
        let first = take(&store, &a, "moved", &objects).await;
        let theirs = take(&store, &b, "moved", &base.join("b")).await;
        b.release("app/moved", &theirs).await;
        a.renew().await;
        let again = loop {
            if let Ok(grant) = a.acquire("app/moved").await {
                break grant;
            }
            a.renew().await;
        };
        a.hold("app/moved", again.clone());
        refused("moved", first.epoch).await;
        assert!(shipped.is_empty(), "shipped by A: {shipped:?} (the last at epoch {} with A's lease at {})", first.epoch, again.epoch);
        std::fs::remove_dir_all(base).unwrap();
    }
}
