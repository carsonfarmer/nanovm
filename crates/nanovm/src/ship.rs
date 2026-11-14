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
//! isn't shipped yet stays, within the capture limit (`max_mb`), which
//! refuses writes once it is reached (the store down a long while, say),
//! never dropping a version. `backlog` says how much waits.
//!
//! A restore (`restore`) rebuilds an object here from the store: its head's
//! version, or an earlier one, which then becomes the version after the
//! head, so the head only moves on. It is staged outside the app's
//! directory, where no worker reaches, then put in place (`install`) once
//! decided, from wherever a crash stopped it (`recover`); meanwhile the
//! object is paused (`pause`), so no instance of it runs. The supervisor
//! (root) reaches what it changes in an app's directory, which the app's
//! worker writes, by name from a descriptor, never through a symlink.

use std::io::{self, ErrorKind};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use object_store::{ObjectStore, ObjectStoreExt, PutMode, UpdateVersion};
use rustix::fs::{AtFlags, CWD, Mode, OFlags, openat, renameat, statat, unlinkat};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::committed::{DIR, dir_at, remove_at};
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
    /// Removes everything under `prefix` (`<app>` or `<app>/<object>`).
    async fn remove(&self, prefix: &str) -> io::Result<()>;
    /// The names one level under `prefix`, each with when it was stored
    /// (seconds since 1970), for what has a time.
    async fn list(&self, prefix: &str) -> io::Result<Vec<(String, Option<u64>)>>;
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
/// write can open: its name is random, and it is created exclusively, never
/// through a symlink. Returns its name.
fn written(dir: &OwnedFd, bytes: &[u8], synced: bool) -> io::Result<String> {
    use std::io::Write;
    let mut name = [0u8; 8];
    // SAFETY: getentropy writes at most `name.len()` bytes into `name`.
    if unsafe { libc::getentropy(name.as_mut_ptr().cast(), name.len()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let name = format!("~new.{:016x}", u64::from_ne_bytes(name));
    let new = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut file = std::fs::File::from(openat(dir, &name, new, Mode::from_raw_mode(0o644))?);
    match file.write_all(bytes).and_then(|()| if synced { file.sync_all() } else { Ok(()) }) {
        Ok(()) => Ok(name),
        Err(e) => Err(unlinkat(dir, &name, AtFlags::empty()).err().map_or(e, io::Error::from)),
    }
}

/// The directory of the object whose versions are in `dir` (`versions`),
/// opened a name at a time from the app's directory, never through a
/// symlink its worker could plant there (B-502); made where missing if
/// `make`.
fn object_at(dir: &Path, make: bool) -> io::Result<OwnedFd> {
    let base = dir.parent().expect("in an object's directory");
    let objects = base.ancestors().nth(2).expect("in an app's shard");
    let under = base.strip_prefix(objects).expect("under the app's directory");
    dir_at(&openat(CWD, objects, DIR, Mode::empty())?, under, make)
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
            let new = dir.join(written(&openat(CWD, dir, DIR, Mode::empty())?, &bytes, true)?);
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
            let new = dir.join(written(&openat(CWD, dir, DIR, Mode::empty())?, &bytes, true)?);
            std::fs::rename(&new, &path)?;
            sync_names(&root, dir, fresh)?;
            Ok(Some(tag(&bytes)))
        })
        .await?
    }

    async fn remove(&self, prefix: &str) -> io::Result<()> {
        match tokio::fs::remove_dir_all(self.0.join(prefix)).await {
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            removed => removed,
        }
    }

    async fn list(&self, prefix: &str) -> io::Result<Vec<(String, Option<u64>)>> {
        let time = |e: &std::fs::DirEntry| e.metadata().ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs());
        Ok(entries(&self.0.join(prefix))?.into_iter().filter_map(|e| Some((e.file_name().into_string().ok()?, time(&e)))).collect())
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

    async fn remove(&self, prefix: &str) -> io::Result<()> {
        use futures::{StreamExt, TryStreamExt};
        let keys = self.0.list(Some(&prefix.into())).map_ok(|m| m.location).boxed();
        self.0.delete_stream(keys).try_collect::<Vec<_>>().await.map(drop).map_err(io::Error::other)
    }

    async fn list(&self, prefix: &str) -> io::Result<Vec<(String, Option<u64>)>> {
        let found = self.0.list_with_delimiter(Some(&prefix.into())).await.map_err(io::Error::other)?;
        let dirs = found.common_prefixes.into_iter().map(|p| (p, None));
        let objects = found.objects.into_iter().map(|o| (o.location, Some(o.last_modified.timestamp() as u64)));
        Ok(dirs.chain(objects).filter_map(|(p, time)| Some((p.filename()?.to_owned(), time))).collect())
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

/// Where object `object` keeps its versions, under its app's `objects`
/// (`<data_dir>/objects/<app>`): `committed` in its directory
/// (`app::object_dir`). With `marker`, `object_at` and `discard`, the only
/// places that know the layout.
pub(crate) fn versions(objects: &Path, object: &str) -> PathBuf {
    crate::committed::directory(&crate::app::object_dir(objects, object))
}

/// Where the shipper records the last version the head has named of the
/// object whose versions are in `dir` (`versions`): `shipped`, beside them.
pub(crate) fn marker(dir: &Path) -> PathBuf {
    dir.with_file_name("shipped")
}

/// Records `sequence` as the last version of the object whose versions are
/// in `dir` that its head has named (`marker`): whole, through a file of its
/// own (claims and the shipper mark objects at once), by name (`object_at`).
/// Unsynced: a marker a crash takes back is caught up from the head (see
/// `ship_object`).
fn mark(dir: &Path, sequence: u64) -> io::Result<()> {
    let object = object_at(dir, true)?;
    let new = written(&object, sequence.to_string().as_bytes(), false)?;
    Ok(renameat(&object, new.as_str(), &object, "shipped")?)
}

/// The last version of the object whose versions are in `dir` that the
/// shipper has seen its head name (0: none).
fn shipped(dir: &Path) -> u64 {
    std::fs::read_to_string(marker(dir)).ok().and_then(|s| s.parse().ok()).unwrap_or(0)
}

/// The directory at `path`, its last name not a symlink.
fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    Ok(openat(CWD, path, DIR, Mode::empty())?)
}

/// Directory `name` in `dir`, made, and given to `uid`, if it isn't there.
fn sub(dir: &OwnedFd, name: &str, uid: Option<u32>) -> io::Result<OwnedFd> {
    let made = match rustix::fs::mkdirat(dir, name, Mode::from_raw_mode(0o755)) {
        Err(Errno::EXIST) => false,
        made => made.map(|()| true)?,
    };
    let at = openat(dir, name, DIR, Mode::empty())?;
    if made && uid.is_some() {
        std::os::unix::fs::fchown(&at, uid, uid)?;
    }
    Ok(at)
}

/// The entries of `dir`, none if it isn't there.
fn entries(dir: &Path) -> io::Result<Vec<std::fs::DirEntry>> {
    match std::fs::read_dir(dir) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(vec![]),
        entries => entries?.collect(),
    }
}

/// Every object's directory under `objects` (`<data_dir>/objects`), with
/// its app's name: `<app>/<shard>/<object>` (`app::object_dir`).
fn object_dirs(objects: &Path) -> Vec<(String, std::fs::DirEntry)> {
    let apps = entries(objects).unwrap_or_default().into_iter();
    apps.flat_map(|app| {
        let name = app.file_name().to_string_lossy().into_owned();
        let shards = entries(&app.path()).unwrap_or_default().into_iter().filter(|s| s.file_name().len() == 2);
        shards.flat_map(|s| entries(&s.path()).unwrap_or_default()).map(move |o| (name.clone(), o))
    })
    .collect()
}

/// How long a commit acknowledged remotely waits for its version to count.
/// The node's time, not the guest's: its deadlines don't cut it short
/// (`object::Phase::Committing`).
pub(crate) const ACK: Duration = Duration::from_secs(2);

/// Waits, `ACK` at most, for the shipper to make the version `sequence` of
/// the object whose versions are in `dir` count: for its head to name it,
/// or a later one.
pub(crate) fn stored(dir: &Path, sequence: u64) -> io::Result<()> {
    let t0 = std::time::Instant::now();
    while shipped(dir) < sequence {
        if t0.elapsed() > ACK {
            return Err(io::Error::new(ErrorKind::TimedOut, format!("version {sequence} is not in the store")));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

/// Whether every version published in `dir` counts in the store, waiting
/// for the shipper as `stored` does. With `remote_ack` an instance starts
/// only once they do: a commit whose wait ran out was answered 500 but
/// stands here, and a node taking the object over before it ships would
/// serve older state than this one had served.
pub(crate) async fn caught_up(dir: &Path) -> io::Result<bool> {
    let (last, dir) = (published(dir)?.0.last().copied().unwrap_or(0), dir.to_owned());
    Ok(shipped(&dir) >= last || tokio::task::spawn_blocking(move || stored(&dir, last)).await?.is_ok())
}

/// The sequences of the versions published in `dir`, in order (none if it
/// isn't there), and whether one is being staged.
fn published(dir: &Path) -> io::Result<(Vec<u64>, bool)> {
    let (versions, staged) = crate::committed::list(dir)?;
    Ok((versions.into_iter().map(|(n, _)| n).collect(), !staged.is_empty()))
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
    let shipped = shipped(dir);
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
                let follows = (n, version.parent.as_str(), version.app.as_str(), version.object.as_str()) == (head.sequence + 1, &head.digest, app, object);
                if !follows || version.epoch > lease {
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
            mark(dir, n)?;
        }
        *known = tag.map(|tag| (head, tag));
    }
    if versions.len() > 1 {
        let here = dir_at(&object_at(dir, false)?, Path::new("committed"), false)?;
        for &n in versions.iter().rev().skip(1) {
            match unlinkat(&here, format!("{n:020}.tar"), AtFlags::empty()) {
                Err(e) if e != Errno::NOENT => return Err(e.into()),
                _ => {}
            }
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

/// What the shipper passes over: apps (`<app>`) and objects
/// (`<app>/<object>`) an operator is deleting or restoring. The shipper
/// holds it to read while it ships an object; adding to it, which takes it
/// to write, waits out what is being shipped.
pub type Held = tokio::sync::RwLock<std::collections::HashSet<String>>;

/// Ships, until the runtime stops, the versions published under `objects`
/// (`<data_dir>/objects`): every app's, every object's, `SHIPPING` objects
/// at once, each in order, but those `held`. A failure is the object's own:
/// logged, and tried again a second later. An object this node holds no
/// live lease on waits, quietly, until it does.
pub async fn run(objects: PathBuf, store: Arc<dyn Store>, leases: Option<Arc<Leases>>, held: Arc<Held>) {
    // None while being shipped: it comes back on `done`.
    let mut known: std::collections::HashMap<PathBuf, Option<Shipping>> = Default::default();
    let (back, mut done) = tokio::sync::mpsc::unbounded_channel();
    let slots = Arc::new(tokio::sync::Semaphore::new(SHIPPING));
    loop {
        while let Ok((dir, state)) = done.try_recv() {
            known.insert(dir, Some(state));
        }
        for (app, object) in object_dirs(&objects) {
            let dir = crate::committed::directory(&object.path());
            let Ok(changed) = dir.metadata().and_then(|m| m.modified()) else { continue };
            let entry = known.entry(dir.clone()).or_insert_with(|| Some(Shipping::default()));
            let Some(state) = entry else { continue };
            // Skip an object nothing was published for since its last round:
            // its directory's time is the same, and older than the clock's tick.
            let changed = Some(changed);
            let old = changed.is_some_and(|t| t.elapsed().is_ok_and(|e| e > Duration::from_millis(100)));
            if (old && state.seen == changed) || state.retry.is_some_and(|t| t > std::time::Instant::now()) {
                continue;
            }
            let mut state = entry.take().expect("not being shipped");
            let id = object.file_name().to_string_lossy().into_owned();
            let (store, leases, slots, back, held) = (store.clone(), leases.clone(), slots.clone(), back.clone(), held.clone());
            tokio::spawn(async move {
                let _slot = slots.acquire().await;
                let held = held.read().await;
                state.retry = None;
                if held.contains(&app) || held.contains(&format!("{app}/{id}")) {
                    let _ = back.send((dir, state));
                    return;
                }
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

/// What waits on this node to be shipped: versions, their bytes, and the
/// objects they are of.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Backlog {
    pub versions: u64,
    pub bytes: u64,
    pub objects: u64,
}

/// The backlog under `objects` (`<data_dir>/objects`): each object's
/// versions after the last its head named, as the shipper recorded it.
pub fn backlog(objects: &Path) -> Backlog {
    let mut out = Backlog::default();
    for (_, object) in object_dirs(objects) {
        let dir = crate::committed::directory(&object.path());
        let shipped = shipped(&dir);
        let versions = crate::committed::list(&dir).map(|v| v.0).unwrap_or_default();
        let sizes: Vec<u64> = versions.into_iter().filter(|v| v.0 > shipped).map(|v| v.1).collect();
        if !sizes.is_empty() {
            (out.objects, out.versions) = (out.objects + 1, out.versions + sizes.len() as u64);
            out.bytes += sizes.iter().sum::<u64>();
        }
    }
    out
}

/// Claims the head of object `object` of app `app` for this node's lease on
/// it, at epoch `lease`, before the object is served here: by a conditional
/// write, after which no node with an earlier lease can move the head. This
/// node's copy of the object (under `objects`, `<data_dir>/objects/<app>`)
/// is then the head's: kept if it is (see `current`), else dropped and
/// restored from the store. So a node never serves state older than the
/// head, nor ships history the head has moved past.
pub async fn claim(store: &dyn Store, node: &str, lease: u64, app: &str, object: &str, objects: &Path) -> io::Result<()> {
    let dir = versions(objects, object);
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
            std::fs::create_dir_all(objects)?;
            discard(objects, object)?;
            sync_fs(objects)?;
        }
        let claimed = Head { node: node.into(), lease, ..head.clone() };
        if store.swap(&head_key(app, object), serde_json::to_vec(&claimed)?, tag.as_ref()).await?.is_none() {
            continue;
        }
        if !current && head.sequence > 0 {
            restore(store, app, object, objects, None, lease).await?;
        }
        return Ok(());
    }
    Err(io::Error::other(format!("{app}/{object}'s head keeps moving")))
}

/// Whether this node's copy of `object` (under `objects`), with versions
/// `here` published (and whether one is being staged), is the head's: it has its
/// volume and the head's version, and versions after it count only if no
/// other node has claimed the object since this one (`node`) did, which
/// another node can't have served. A copy with versions and no volume is
/// one a discard cut short.
fn current(objects: &Path, object: &str, (here, staged): (Vec<u64>, bool), head: &Head, node: &str) -> io::Result<bool> {
    let at = versions(objects, object).join(format!("{:020}.tar", head.sequence));
    let whole = (here.is_empty() && !staged) || crate::app::object_dir(objects, object).join(crate::journal::VOLUME).is_dir();
    let has = head.sequence == 0 || (here.contains(&head.sequence) && crate::committed::digest(&at)? == head.digest);
    let ours = head.node == node || head.lease == 0;
    Ok(whole && has && (ours || (!staged && here.last().is_none_or(|&n| n == head.sequence))))
}

/// Drops this node's copy of `object` (under `objects`): its volume first,
/// which a copy with versions has (see `current`), then its journal and
/// marker, and its versions last; so a discard cut short is never taken
/// for a current copy. By name from the app's directory, never through a
/// symlink its worker could plant there (B-502).
fn discard(objects: &Path, object: &str) -> io::Result<()> {
    let base = match object_at(&versions(objects, object), false) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        base => base?,
    };
    for name in [crate::journal::VOLUME, "journal", "shipped", "committed"] {
        remove_at(&base, name.as_ref())?;
    }
    Ok(())
}

/// What a restore did: the version of the object it restored, and its head
/// after (one past the head before, for an older version).
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Restored {
    pub object: String,
    pub version: u64,
    pub head: u64,
}

/// Where a restore of `object` (under `objects`, `<data_dir>/objects/<app>`)
/// is staged: `<data_dir>/restore/<app>/<object>`, which no worker reaches.
fn staging(objects: &Path, object: &str) -> PathBuf {
    let data = objects.parent().and_then(Path::parent).expect("in <data_dir>/objects");
    data.join("restore").join(objects.file_name().expect("an app's")).join(object)
}

/// The uid that owns `objects` (an app's directory) when this process is
/// root, which gives what it makes there to the app.
fn owner(objects: &Path) -> io::Result<Option<u32>> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: no arguments, no memory.
    Ok((unsafe { libc::geteuid() } == 0).then(|| std::fs::metadata(objects).map(|m| m.uid())).transpose()?)
}

/// Gives `path`, and all in it, to `uid`: a tree only this process reaches.
fn own(path: &Path, uid: u32) -> io::Result<()> {
    std::os::unix::fs::lchown(path, Some(uid), Some(uid))?;
    if std::fs::symlink_metadata(path)?.is_dir() {
        for entry in std::fs::read_dir(path)? {
            own(&entry?.path(), uid)?;
        }
    }
    Ok(())
}

/// Rebuilds object `object` of app `app` here (under `objects`,
/// `<data_dir>/objects/<app>`) as version `to` of it in `store`, or as its
/// head's: fetched back from the head, each version the parent the one
/// after it names and checked against its digest and its identity, to `to`
/// and on to the last snapshot before it. The head's is kept so history
/// goes on from it. An older version is restored as a new one after the
/// head, under lease epoch `epoch`, so the head only moves on (once it is
/// shipped) and the versions after the one restored stay in the store. The
/// object's volume, journal and versions here are replaced by `install`. A
/// crash before the restore is decided leaves the object as it was; one
/// after, `recover` finishes.
pub async fn restore(store: &dyn Store, app: &str, object: &str, objects: &Path, to: Option<u64>, epoch: u64) -> io::Result<Restored> {
    let (head, _) = head(store, app, object).await?;
    if head.sequence == 0 {
        return Err(io::Error::new(ErrorKind::NotFound, format!("nothing of {app}/{object} counts in the store")));
    }
    let to = to.unwrap_or(head.sequence);
    if !(1..=head.sequence).contains(&to) {
        return Err(io::Error::new(ErrorKind::InvalidInput, format!("{app}/{object} has versions 1 to {}", head.sequence)));
    }
    std::fs::create_dir_all(objects)?;
    let stage = staging(objects, object);
    // What a restore cut short left.
    if let Err(e) = std::fs::remove_dir_all(&stage)
        && e.kind() != ErrorKind::NotFound
    {
        return Err(e);
    }
    let (versions, history) = (stage.join("versions"), stage.join("history"));
    std::fs::create_dir_all(&versions)?;
    std::fs::create_dir(&history)?;
    let (mut n, mut digest) = (head.sequence, head.digest.clone());
    loop {
        let missing = || io::Error::new(ErrorKind::NotFound, format!("{app}/{object} version {n} is not in the store"));
        let bytes = store.get(&version_key(app, object, n, &digest)).await?.ok_or_else(missing)?;
        let version = crate::committed::identity(&bytes[..])?;
        if hex(&bytes) != digest || (version.sequence, version.app.as_str(), version.object.as_str()) != (n, app, object) {
            return Err(io::Error::new(ErrorKind::InvalidData, format!("{app}/{object} version {n} isn't what its key names")));
        }
        let file = |dir: &Path| dir.join(format!("{n:020}.tar"));
        if n == head.sequence && n > to {
            std::fs::write(file(&history), &bytes)?;
        }
        if n <= to {
            std::fs::write(file(&versions), &bytes)?;
            if version.snapshot || n == 1 {
                break;
            }
        }
        (n, digest) = (n - 1, version.parent);
    }
    #[cfg(test)]
    tests::cut()?;
    let volume = stage.join("volume");
    crate::committed::restore(&versions, &volume)?;
    if to == head.sequence {
        std::fs::rename(versions.join(format!("{to:020}.tar")), history.join(format!("{to:020}.tar")))?;
    } else {
        let obj = crate::object::ObjShared::new(app.into(), object);
        let cap = CaptureConfig { max_mb: u64::MAX >> 20, ..CaptureConfig::default() };
        crate::committed::prepare(&volume, &history, &obj, epoch, &cap, &[0; 8], None)?;
        crate::committed::publish(&history, &[0; 8])?;
    }
    std::fs::remove_dir_all(&versions)?;
    if let Some(uid) = owner(objects)? {
        own(&volume, uid)?;
        own(&history, uid)?;
    }
    // Decided once all of it is durable.
    sync_fs(&stage)?;
    #[cfg(test)]
    tests::cut()?;
    std::fs::File::create(stage.join("ready"))?;
    crate::journal::sync(&stage)?;
    install(objects, object)?;
    Ok(Restored { object: object.into(), version: to, head: head.sequence + u64::from(to < head.sequence) })
}

/// Puts what a restore of `object` staged and decided in place under
/// `objects` (see `restore`): its volume and versions, the first of them
/// marked shipped, the journal of the volume they replace gone. By name
/// from the app's directory, the object's directory made (and given to the
/// app) where missing. Each step can be taken again, so a crash in the
/// middle is finished by `recover`.
fn install(objects: &Path, object: &str) -> io::Result<()> {
    let stage = staging(objects, object);
    let at = open_dir(&stage)?;
    let staged = |name: &str| statat(&at, name, AtFlags::SYMLINK_NOFOLLOW).is_ok();
    #[cfg(test)]
    let cut = tests::cut;
    #[cfg(not(test))]
    let cut = || io::Result::Ok(());
    let (uid, place) = (owner(objects)?, crate::app::object_dir(objects, object));
    let shard = place.parent().and_then(Path::file_name).and_then(|s| s.to_str()).expect("an object's shard");
    let base = sub(&sub(&open_dir(objects)?, shard, uid)?, object, uid)?;
    cut()?;
    remove_at(&base, "journal".as_ref())?;
    cut()?;
    if staged("volume") {
        remove_at(&base, crate::journal::VOLUME.as_ref())?;
        cut()?;
        renameat(&at, "volume", &base, crate::journal::VOLUME)?;
    }
    cut()?;
    if staged("history") {
        let first = published(&stage.join("history"))?.0.first().copied();
        let first = first.ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "a restore without versions"))?;
        mark(&versions(objects, object), first)?;
        cut()?;
        remove_at(&base, "committed".as_ref())?;
        cut()?;
        renameat(&at, "history", &base, "committed")?;
    }
    cut()?;
    sync_fs(objects)?;
    std::fs::remove_dir_all(stage)
}

/// Before any worker starts: finishes each restore a crash cut short once it
/// was decided, drops the others, and resumes every paused object. `data`
/// is the node's `data_dir`.
pub fn recover(data: &Path) -> io::Result<()> {
    for app in entries(&data.join("restore"))? {
        for object in entries(&app.path())? {
            match object.path().join("ready").exists() {
                true => install(&data.join("objects").join(app.file_name()), &object.file_name().to_string_lossy())?,
                false => std::fs::remove_dir_all(object.path())?,
            }
        }
    }
    for app in entries(&data.join("objects"))?.into_iter().filter(|a| a.file_type().is_ok_and(|t| t.is_dir())) {
        remove_at(&open_dir(&app.path())?, "~paused".as_ref())?;
    }
    Ok(())
}

/// Pauses `object` (under `objects`, an app's) for a restore: no instance
/// of it starts, on any worker of the app, until `resume`. Refused if it is
/// paused already.
pub fn pause(objects: &Path, object: &str) -> io::Result<()> {
    let paused = sub(&open_dir(objects)?, "~paused", None)?;
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match openat(&paused, object, flags, Mode::from_raw_mode(0o644)) {
        Err(Errno::EXIST) => Err(io::Error::new(ErrorKind::AlreadyExists, format!("{object} is being restored already"))),
        made => made.map(drop).map_err(Into::into),
    }
}

pub fn resume(objects: &Path, object: &str) -> io::Result<()> {
    match openat(&open_dir(objects)?, "~paused", DIR, Mode::empty()) {
        Err(Errno::NOENT) => Ok(()),
        paused => remove_at(&paused?, object.as_ref()),
    }
}

/// Whether object `id` (under `objects`, an app's) is paused for a restore.
pub fn paused(objects: &Path, id: &str) -> bool {
    objects.join("~paused").join(id).exists()
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

    /// Object `o`'s volume under `objects`.
    fn vol(objects: &Path, o: &str) -> PathBuf {
        crate::app::object_dir(objects, o).join(crate::journal::VOLUME)
    }

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

    thread_local! {
        /// Cut points (see `cut`) to pass before one fails, as a crash there.
        static CUT: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    }

    /// A point where a restore may be cut short, as by a crash: the
    /// `CUT`-th one fails.
    pub(super) fn cut() -> io::Result<()> {
        CUT.with(|c| match c.get() {
            Some(0) => Err(io::Error::other("cut")),
            n => Ok(c.set(n.map(|n| n - 1))),
        })
    }

    /// A restore cut short at each point, then the node restarted
    /// (`recover`): before it was decided, the object is as it was, journal
    /// and all; after, it is restored, as the version after the head, which
    /// then ships. Either way nothing is left staged and nothing paused.
    #[tokio::test]
    async fn a_restore_cut_short_is_finished_or_undone() {
        let base = tempdir("cut");
        let store = Dir(base.join("store"));
        let (data, objects) = (base.join("node"), base.join("node/objects/app"));
        let dir = versions(&objects, "o");
        let journal = crate::app::object_dir(&objects, "o").join("journal");
        let text = || std::fs::read_to_string(vol(&objects, "o").join("v")).unwrap();
        for cut in 0.. {
            let _ = std::fs::remove_dir_all(&base);
            let mut parent = String::new();
            for n in 1..=3 {
                parent = version(&dir, n, 0, &parent, &format!("v{n}"));
                ship_object(&store, None, "app", "o", &dir, &mut None).await.unwrap();
            }
            std::fs::create_dir_all(vol(&objects, "o")).unwrap();
            std::fs::write(vol(&objects, "o").join("v"), "v3").unwrap();
            std::fs::create_dir_all(&journal).unwrap();
            std::fs::write(journal.join("log"), "the old volume's").unwrap();
            pause(&objects, "o").unwrap();
            CUT.set(Some(cut));
            let restored = restore(&store, "app", "o", &objects, Some(2), 0).await;
            CUT.set(None);
            recover(&data).unwrap();
            assert!(!paused(&objects, "o") && !base.join("node/restore/app/o").exists(), "cut {cut}");
            let decided = restored.is_ok() || cut >= 2;
            let history = published(&dir).unwrap().0;
            match decided {
                false => assert_eq!((text(), journal.join("log").exists(), history), ("v3".into(), true, vec![3]), "cut {cut}"),
                true => assert_eq!((text(), journal.join("log").exists(), history), ("v2".into(), false, vec![3, 4]), "cut {cut}"),
            }
            ship_object(&store, None, "app", "o", &dir, &mut None).await.unwrap();
            assert_eq!(head(&store, "app", "o").await.unwrap().0.sequence, 3 + u64::from(decided), "cut {cut}");
            if let Ok(restored) = restored {
                assert_eq!(restored, Restored { object: "o".into(), version: 2, head: 4 });
                assert_eq!(cut, 9, "cut points");
                break;
            }
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    /// What an operator holds still for a delete or a restore (`Held`: an
    /// object, or a whole app) isn't shipped meanwhile; the rest is, and it
    /// is once let go.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_shipper_passes_what_is_held_over() {
        let base = tempdir("held");
        let (objects, store) = (base.join("objects"), Arc::new(Dir(base.join("store"))));
        let dir = |o: &str| versions(&objects.join("app"), o);
        let held: Arc<Held> = Default::default();
        held.write().await.insert("app/o".into());
        let one = [version(&dir("o"), 1, 0, "", "o1"), version(&dir("p"), 1, 0, "", "p1")];
        let shipper = tokio::spawn(run(objects.clone(), store.clone(), None, held.clone()));
        let at = async |o: &str| head(&*store, "app", o).await.unwrap().0.sequence;
        let until = async |o: &str, n: u64| {
            let t0 = std::time::Instant::now();
            while at(o).await < n {
                assert!(t0.elapsed() < Duration::from_secs(5), "{o} never reached {n}");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        until("p", 1).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(at("o").await, 0, "a held object shipped");
        // The whole app held: neither ships.
        held.write().await.insert("app".into());
        version(&dir("p"), 2, 0, &one[1], "p2");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!((at("o").await, at("p").await), (0, 1), "a held app shipped");
        held.write().await.clear();
        until("o", 1).await;
        until("p", 2).await;
        shipper.abort();
        std::fs::remove_dir_all(base).unwrap();
    }

    /// A version that names another app or object is never shipped as this
    /// one's, wherever it is found (a worker can put anything in its own
    /// directory).
    #[tokio::test]
    async fn a_version_naming_another_object_is_not_shipped() {
        let base = tempdir("identity");
        let store = Dir(base.join("store"));
        let objects = base.join("objects/app");
        let (theirs, dir) = (versions(&objects, "x"), versions(&objects, "o"));
        version(&theirs, 1, 0, "", "x's");
        assert!(ship_object(&store, None, "other", "x", &theirs, &mut None).await.is_err(), "another app's");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::rename(theirs.join(format!("{:020}.tar", 1)), dir.join(format!("{:020}.tar", 1))).unwrap();
        assert!(ship_object(&store, None, "app", "o", &dir, &mut None).await.is_err(), "another object's");
        assert_eq!(head(&store, "app", "o").await.unwrap().0.sequence, 0);
        std::fs::remove_dir_all(base).unwrap();
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
        async fn remove(&self, prefix: &str) -> io::Result<()> {
            self.store.as_ref().unwrap().remove(prefix).await
        }
        async fn list(&self, prefix: &str) -> io::Result<Vec<(String, Option<u64>)>> {
            self.store.as_ref().unwrap().list(prefix).await
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
        let objects = base.join("a/objects/app");
        let dir = versions(&objects, "o");
        let commit = |n: u64, parent: &str, v: &str| {
            std::fs::create_dir_all(vol(&objects, "o")).unwrap();
            std::fs::write(vol(&objects, "o").join("v"), v).unwrap();
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
        let text = std::fs::read_to_string(vol(&objects, "o").join("v")).map_err(|e| e.to_string());
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
        let objects = base.join("a/objects/app");
        version(&versions(&objects, "o"), 1, 1, "", "one");
        std::fs::create_dir_all(vol(&objects, "o")).unwrap();
        std::fs::write(vol(&objects, "o").join("v"), "one").unwrap();
        let theirs = Head { sequence: 0, digest: String::new(), node: "b".into(), lease: 2 };
        store.swap(&head_key("app", "o"), serde_json::to_vec(&theirs).unwrap(), None).await.unwrap().unwrap();
        FAIL_SYNC.set(true);
        let claimed = claim(&store, "a", 3, "app", "o", &objects).await;
        FAIL_SYNC.set(false);
        assert!(claimed.is_err(), "claimed though the drop can't be made durable");
        assert_eq!(head(&store, "app", "o").await.unwrap().0, theirs);
        claim(&store, "a", 3, "app", "o", &objects).await.unwrap();
        assert_eq!(head(&store, "app", "o").await.unwrap().0.node, "a");
        assert!(!vol(&objects, "o").exists() && published(&versions(&objects, "o")).unwrap().0.is_empty());
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
        let object = dir.parent().unwrap().file_name().unwrap().to_str().unwrap();
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
        restore(store, "app", object, root, None, 0).await.map_err(|e| e.to_string())?;
        Ok(std::fs::read_to_string(vol(root, object).join("v")).unwrap())
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
        let (store, dir) = (Dir(base.join("store")), versions(&base.join("objects/app"), "o"));
        let local = || published(&dir).unwrap().0;
        let mut parent = String::new();
        for n in 1..=3 {
            parent = version(&dir, n, 0, &parent, &format!("v{n}"));
        }
        ship_object(&store, None, "app", "o", &dir, &mut None).await.unwrap();
        let shipped = Head { sequence: 3, digest: parent.clone(), node: "".into(), lease: 0 };
        assert_eq!(head(&store, "app", "o").await.unwrap().0, shipped);
        assert_eq!((local(), std::fs::read_to_string(marker(&dir)).unwrap()), (vec![3], "3".into()));
        assert_eq!(restored(&store, "o", &base.join("fresh/objects/app")).await.unwrap(), "v3");
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
        async fn remove(&self, prefix: &str) -> io::Result<()> {
            self.store.remove(prefix).await
        }
        async fn list(&self, prefix: &str) -> io::Result<Vec<(String, Option<u64>)>> {
            self.store.list(prefix).await
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
        let (a_objects, b_objects) = (base.join("a/objects/app"), base.join("b/objects/app"));
        let (a_dir, b_dir) = (versions(&a_objects, "o"), versions(&b_objects, "o"));
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
        assert_eq!(restored(&store, "o", &base.join("fresh/objects/app")).await.unwrap(), "B's");
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
        let (a_objects, b_objects) = (base.join("a/objects/app"), base.join("b/objects/app"));
        let dir = versions(&a_objects, "o");
        let text = |objects: &Path| std::fs::read_to_string(vol(&objects, "o").join("v")).ok();
        // A version and the volume as it leaves it.
        let commit = |n: u64, epoch: u64, parent: &str, v: &str| {
            std::fs::create_dir_all(vol(&a_objects, "o")).unwrap();
            std::fs::write(vol(&a_objects, "o").join("v"), v).unwrap();
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
            let dir = versions(&base.join("a/objects/app"), &object);
            let digest = version(&dir, n, 1, "", &format!("v{i}"));
            let bytes = std::fs::read(dir.join(format!("{n:020}.tar"))).unwrap();
            store.put(&version_key("app", &object, n, &digest), bytes).await.unwrap();
            let head = Head { sequence: n, digest, node: "a".into(), lease: 1 };
            store.swap(&head_key("app", &object), serde_json::to_vec(&head).unwrap(), None).await.unwrap().unwrap();
        }
        let mut wrong = vec![];
        for round in 0..20u64 {
            // Node b, on a disk of its own, takes every object at once.
            let objects = base.join(format!("b{round}/objects/app"));
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
                let dir = versions(&objects, &format!("o{i}"));
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
        let objects = base.join("a/objects/app");
        let dir = versions(&objects, "o");
        let text = || std::fs::read_to_string(vol(&objects, "o").join("v")).map_err(|e| e.to_string());
        // A shipped version 1, committed 2 (unshipped), then B took the
        // object over at epoch 2 and claimed its head.
        let one = version(&dir, 1, 1, "", "one");
        store.put(&version_key("app", "o", 1, &one), std::fs::read(dir.join(format!("{:020}.tar", 1))).unwrap()).await.unwrap();
        let head = Head { sequence: 1, digest: one.clone(), node: "b".into(), lease: 2 };
        store.swap(&head_key("app", "o"), serde_json::to_vec(&head).unwrap(), None).await.unwrap().unwrap();
        version(&dir, 2, 1, &one, "two");
        mark(&dir, 1).unwrap();
        std::fs::create_dir_all(vol(&objects, "o")).unwrap();
        std::fs::write(vol(&objects, "o").join("v"), "two").unwrap();
        // A's claim at epoch 3 is cut short: the volume went, then version 2.
        // Made by hand, so the test doesn't depend on the order a filesystem
        // lists a directory in (the review's ext4 listed the newest first, so
        // the old `remove_dir_all` left exactly this; CI's filesystem doesn't).
        std::fs::remove_dir_all(vol(&objects, "o")).unwrap();
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
        async fn remove(&self, prefix: &str) -> io::Result<()> {
            self.store.remove(prefix).await
        }
        async fn list(&self, prefix: &str) -> io::Result<Vec<(String, Option<u64>)>> {
            self.store.list(prefix).await
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
        let dirs: Vec<_> = (0..OBJECTS).map(|i| versions(&objects.join("app"), &format!("o{i}"))).collect();
        for dir in &dirs {
            version(dir, 1, 0, "", "v");
        }
        let store = Arc::new(Latent { store: Objects(Arc::new(object_store::memory::InMemory::new())), delay: Duration::from_millis(25), reads: Default::default() });
        let shipper = tokio::spawn(run(objects, store, None, Default::default()));
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

    /// The supervisor (root) marks, drops and prunes an object's copy by
    /// name from the app's directory, which the app's worker writes: a
    /// symlink it plants there (the object's directory, or its `committed`)
    /// is refused, never followed, so nothing outside changes (B-502).
    #[tokio::test]
    async fn the_supervisor_never_follows_a_symlink_the_app_planted() {
        let base = tempdir("planted");
        let (objects, outside) = (base.join("a"), base.join("outside"));
        let listing = |d: &Path| {
            let mut names: Vec<_> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
            names.sort();
            names
        };
        // Outside: what a symlink leads to, an object's two versions.
        let one = version(&outside.join("o/committed"), 1, 0, "", "theirs");
        version(&outside.join("o/committed"), 2, 0, &one, "theirs");
        let at = crate::app::object_dir(&objects, "o");
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(outside.join("o"), &at).unwrap();
        let before = (listing(&outside.join("o")), listing(&outside.join("o/committed")));
        assert!(mark(&versions(&objects, "o"), 3).is_err(), "marked through the object's directory");
        assert!(discard(&objects, "o").is_err(), "dropped through the object's directory");
        // The shipper, marking in a real directory, prunes nothing through
        // its `committed`.
        std::fs::remove_file(&at).unwrap();
        std::fs::create_dir(&at).unwrap();
        std::os::unix::fs::symlink(outside.join("o/committed"), at.join("committed")).unwrap();
        let store = Objects(Arc::new(object_store::memory::InMemory::new()));
        let pruned = ship_object(&store, None, "app", "o", &versions(&objects, "o"), &mut None).await;
        assert!(pruned.is_err(), "pruned through committed");
        assert_eq!((listing(&outside.join("o")), listing(&outside.join("o/committed"))), before, "changed outside the app's directory");
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
            async fn remove(&self, prefix: &str) -> io::Result<()> {
                self.0.remove(prefix).await
            }
            async fn list(&self, prefix: &str) -> io::Result<Vec<(String, Option<u64>)>> {
                self.0.list(prefix).await
            }
        }
        let base = tempdir("denied");
        let objects = base.join("objects");
        let dir = |o: &str| versions(&objects.join("app"), o);
        version(&dir("bad"), 1, 0, "", "v");
        let store = Arc::new(Denying(Objects(Arc::new(object_store::memory::InMemory::new())), Default::default()));
        let shipper = tokio::spawn(run(objects.clone(), store.clone(), None, Default::default()));
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
        let (store, dir) = (Objects(Arc::new(object_store::memory::InMemory::new())), versions(&base.join("objects/app"), "o"));
        let mut parents = vec![String::new()];
        for n in 1..=3 {
            parents.push(version(&dir, n, 0, &parents[n as usize - 1], &format!("v{n}")));
        }
        ship_object(&store, None, "app", "o", &dir, &mut None).await.unwrap();
        for n in 2..=3 {
            version(&dir, n, 0, &parents[n as usize - 1], &format!("v{n}"));
        }
        mark(&dir, 1).unwrap();
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
            rt.spawn(run(base.join("objects"), store, None, Default::default()));
            let (acked, late, cpu0, t0) = (AtomicU64::new(0), AtomicU64::new(0), cpu(), std::time::Instant::now());
            std::thread::scope(|s| {
                for i in 0..n {
                    let (dir, acked, late) = (versions(&base.join("objects/app"), &format!("o{i}")), &acked, &late);
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
        let dir = versions(&base.join("objects/app"), "o");
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
        let (a_objects, b_objects) = (base.join("a/objects/app"), base.join("b/objects/app"));
        let ga = take(&store, &a, "o", &a_objects).await;
        let (id, until) = a.renew().await;
        let session = Arc::new(crate::lease::Session::default());
        session.set(id, until);
        let mut obj = crate::object::ObjShared::new("app".into(), "o");
        (obj.leases, *obj.lease.get_mut()) = (Some(session), Some(ga));
        let obj = Arc::new(obj);
        let volume = vol(&a_objects, "o");
        std::fs::create_dir_all(&volume).unwrap();
        std::fs::write(volume.join("v"), "zero").unwrap();
        let volume = volume.canonicalize().unwrap();
        // Writes `text` over `v` and commits, as a request does.
        let commit = |remote_ack: bool, text: &str| {
            let cap = CaptureConfig { remote_ack, store: Some("/unused".into()), ..Default::default() };
            let mut j = Journal::open(&volume, 1 << 30, obj.clone(), Some(&cap), None).unwrap();
            let file = std::fs::File::options().write(true).open(volume.join("v")).unwrap();
            j.writing(&file, 0, text.len() as u64).unwrap();
            file.write_all_at(text.as_bytes(), 0).unwrap();
            j.commit()
        };
        // "zero" counts in the store.
        commit(false, "zero").unwrap();
        ship_object(&store, Some(&a), "app", "o", &versions(&a_objects, "o"), &mut None).await.unwrap();
        // "one!" is committed, but the store is slow: answered 500.
        assert!(commit(true, "one!").is_err());
        // A's next instance recovers the journal, then, as the node starts
        // one (`object::run`, acknowledging remotely), serves the object
        // only once all of it counts in the store: not yet, so A serves
        // nothing; the last it served was "zero".
        let cap = CaptureConfig { remote_ack: true, store: Some("/unused".into()), ..Default::default() };
        drop(Journal::open(&volume, 1 << 30, obj.clone(), Some(&cap), None).unwrap());
        let a_dir = versions(&a_objects, "o");
        assert!(!caught_up(&a_dir).await.unwrap(), "A serves what it answered 500 for, which the store doesn't have");
        let served = "zero";
        // A stops; B takes the object over.
        tokio::time::sleep(Duration::from_millis(500)).await;
        take(&store, &b, "o", &b_objects).await;
        let theirs = std::fs::read_to_string(vol(&b_objects, "o").join("v")).unwrap();
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
        let objects = base.join("a/objects/app");
        // A has a version of `object`, committed at `epoch`; it must not count.
        let mut shipped = vec![];
        let mut refused = async |object: &str, epoch: u64| {
            let dir = versions(&objects, object);
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
        let theirs = take(&store, &b, "moved", &base.join("b/objects/app")).await;
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
