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
use rustix::fs::{AtFlags, CWD, Mode, OFlags, openat, renameat, statat};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::committed::{DIR, remove_at};
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

/// Writes `bytes`, synced, to a new file in `dir` that no other write can
/// open: its name is random and it is created exclusively. Returns its path.
fn written(dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    use std::io::Write;
    let mut name = [0u8; 8];
    // SAFETY: getentropy writes at most `name.len()` bytes into `name`.
    if unsafe { libc::getentropy(name.as_mut_ptr().cast(), name.len()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let path = dir.join(format!("~new.{:016x}", u64::from_ne_bytes(name)));
    let mut file = std::fs::File::options().write(true).create_new(true).open(&path)?;
    match file.write_all(bytes).and_then(|()| file.sync_all()) {
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
            let new = written(dir, &bytes)?;
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
            let new = written(dir, &bytes)?;
            std::fs::rename(&new, &path)?;
            sync_names(&root, dir, fresh)?;
            Ok(Some(tag(&bytes)))
        })
        .await?
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

/// Where the shipper records the last version the head has named of the
/// object whose versions are in `dir` (`<objects>/~committed/<object>`):
/// `<objects>/~shipped/<object>`.
pub(crate) fn marker(dir: &Path) -> PathBuf {
    let objects = dir.parent().and_then(Path::parent).expect("in ~committed");
    objects.join("~shipped").join(dir.file_name().expect("an object's"))
}

/// Records `sequence` in `marker`, whole.
fn mark(marker: &Path, sequence: u64) -> io::Result<()> {
    let new = marker.with_file_name("~new");
    std::fs::create_dir_all(marker.parent().expect("in ~shipped"))?;
    std::fs::write(&new, sequence.to_string())?;
    std::fs::rename(new, marker)
}

/// The directory at `path`, its last name not a symlink.
fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    Ok(openat(CWD, path, DIR, Mode::empty())?)
}

/// Directory `name` in `app` (an app's directory), made, and given to `uid`,
/// if it isn't there.
fn sub(app: &OwnedFd, name: &str, uid: Option<u32>) -> io::Result<OwnedFd> {
    let made = match rustix::fs::mkdirat(app, name, Mode::from_raw_mode(0o755)) {
        Err(Errno::EXIST) => false,
        made => made.map(|()| true)?,
    };
    let dir = openat(app, name, DIR, Mode::empty())?;
    if made && uid.is_some() {
        std::os::unix::fs::fchown(&dir, uid, uid)?;
    }
    Ok(dir)
}

/// Removes `name`, and all in it, from directory `dir` of `app`.
fn remove_in(app: &OwnedFd, dir: &str, name: &str) -> io::Result<()> {
    match openat(app, dir, DIR, Mode::empty()) {
        Err(Errno::NOENT) => Ok(()),
        dir => remove_at(&dir?, name.as_ref()),
    }
}

/// The entries of `dir`, none if it isn't there.
fn entries(dir: &Path) -> io::Result<Vec<std::fs::DirEntry>> {
    match std::fs::read_dir(dir) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(vec![]),
        entries => entries?.collect(),
    }
}

/// Waits, two seconds at most (within an instance's post-return deadline),
/// for the shipper to make the version `sequence` of the object whose
/// versions are in `dir` count: for its head to name it, or a later one.
pub(crate) fn stored(dir: &Path, sequence: u64) -> io::Result<()> {
    for _ in 0..1000 {
        if std::fs::read_to_string(marker(dir)).ok().and_then(|s| s.parse::<u64>().ok()) >= Some(sequence) {
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
/// node, there are no leases: "" and 0).
fn authority<'a>(leases: Option<&'a Leases>, app: &str, object: &str) -> io::Result<(&'a str, u64)> {
    let Some(leases) = leases else { return Ok(("", 0)) };
    let held = leases.epoch(&format!("{app}/{object}")).map(|epoch| (leases.node(), epoch));
    held.ok_or_else(|| io::Error::new(ErrorKind::PermissionDenied, format!("this node holds no live lease on {app}/{object}")))
}

/// Ships what the object whose versions are in `dir` hasn't yet, in order,
/// moving its head to each, then removes the shipped ones here but the
/// last. Only under this node's live lease on the object (`authority`), once
/// it has claimed the head with it; and only versions that follow the head,
/// so a node never ships history the head has moved past.
pub(crate) async fn ship_object(store: &dyn Store, leases: Option<&Leases>, app: &str, object: &str, dir: &Path) -> io::Result<()> {
    let marker = marker(dir);
    let shipped: u64 = std::fs::read_to_string(&marker).ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let (versions, _) = published(dir)?;
    let unshipped: Vec<_> = versions.iter().copied().filter(|&n| n > shipped).collect();
    if !unshipped.is_empty() {
        let (node, lease) = authority(leases, app, object)?;
        let (mut head, mut tag) = head(store, app, object).await?;
        if (head.node.as_str(), head.lease) != (node, lease) {
            return Err(io::Error::other(format!("{app}/{object}'s head is claimed by {:?} at epoch {}", head.node, head.lease)));
        }
        for n in unshipped {
            let bytes = tokio::fs::read(dir.join(format!("{n:020}.tar"))).await?;
            let digest = hex(&bytes);
            // Unless the head names it already (it was shipped, not marked).
            if (n, &digest) != (head.sequence, &head.digest) {
                let version = crate::committed::identity(&bytes[..])?;
                let follows = (n, version.parent.as_str(), version.app.as_str(), version.object.as_str()) == (head.sequence + 1, &head.digest, app, object);
                if !follows || version.epoch > lease {
                    return Err(io::Error::other(format!("{app}/{object} version {n} doesn't follow its head, {}", head.sequence)));
                }
                if let Put::Conflict = store.put(&version_key(app, object, n, &digest), bytes).await? {
                    return Err(io::Error::other(format!("{app}/{object} version {n} isn't what its key names")));
                }
                let next = Head { sequence: n, digest, ..head.clone() };
                let swapped = store.swap(&head_key(app, object), serde_json::to_vec(&next)?, tag.as_ref()).await?;
                tag = Some(swapped.ok_or_else(|| io::Error::other(format!("{app}/{object}'s head moved")))?);
                head = next;
            }
            mark(&marker, n)?;
        }
    }
    for &n in versions.iter().rev().skip(1) {
        match std::fs::remove_file(dir.join(format!("{n:020}.tar"))) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// Ships, until the runtime stops, the versions published under `objects`
/// (`<data_dir>/objects`): every app's, every object's, retrying failures
/// a second later. An object this node holds no live lease on waits, quietly,
/// until it does.
pub async fn run(objects: PathBuf, store: Arc<dyn Store>, leases: Option<Arc<Leases>>) {
    let mut seen = std::collections::HashMap::new();
    loop {
        let dirs = std::fs::read_dir(&objects).into_iter().flatten().flatten().flat_map(|app| {
            let committed = app.path().join("~committed");
            let objects = std::fs::read_dir(committed).into_iter().flatten().flatten();
            objects.filter(|o| o.file_type().is_ok_and(|t| t.is_dir())).map(move |o| (app.file_name(), o))
        });
        let mut failed = false;
        for (app, object) in dirs.collect::<Vec<_>>() {
            // Skip an object nothing was published for since its last round:
            // its directory's time is the same, and older than the clock's tick.
            let changed = object.metadata().and_then(|m| m.modified()).ok();
            let old = changed.is_some_and(|t| t.elapsed().is_ok_and(|e| e > Duration::from_millis(100)));
            if old && seen.get(&object.path()) == Some(&changed) {
                continue;
            }
            let (app, id) = (app.to_string_lossy(), object.file_name().to_string_lossy().into_owned());
            match ship_object(&*store, leases.as_deref(), &app, &id, &object.path()).await {
                Ok(()) => _ = seen.insert(object.path(), changed),
                Err(e) if e.kind() == ErrorKind::PermissionDenied => {}
                Err(e) => {
                    eprintln!("nanovm: shipping {app}/{id}: {e}");
                    failed = true;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(if failed { 1000 } else { 20 })).await;
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
    for app in entries(objects).unwrap_or_default() {
        for object in entries(&app.path().join("~committed")).unwrap_or_default() {
            let dir = object.path();
            let shipped: u64 = std::fs::read_to_string(marker(&dir)).ok().and_then(|s| s.parse().ok()).unwrap_or(0);
            let waiting = published(&dir).map(|v| v.0).unwrap_or_default().into_iter().filter(|&n| n > shipped);
            let sizes: Vec<u64> = waiting.filter_map(|n| std::fs::metadata(dir.join(format!("{n:020}.tar"))).ok()).map(|m| m.len()).collect();
            if !sizes.is_empty() {
                (out.objects, out.versions) = (out.objects + 1, out.versions + sizes.len() as u64);
                out.bytes += sizes.iter().sum::<u64>();
            }
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
    let dir = objects.join("~committed").join(object);
    for _ in 0..4 {
        let (head, tag) = head(store, app, object).await?;
        if head.lease > lease || (head.lease == lease && head.node != node) {
            return Err(io::Error::other(format!("{app}/{object} is claimed at a later epoch, {}", head.lease)));
        }
        // Dropped before the claim, which makes what is here count.
        let current = current(&dir, &head, node)?;
        if !current {
            discard(objects, object)?;
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

/// Whether this node's copy of an object, its versions in `dir`, is the
/// head's: it has the head's version, and versions after it count only if
/// no other node has claimed the object since this one (`node`) did, which
/// another node can't have served.
fn current(dir: &Path, head: &Head, node: &str) -> io::Result<bool> {
    let (versions, staged) = published(dir)?;
    let at = |n: u64| dir.join(format!("{n:020}.tar"));
    let has = head.sequence == 0 || (versions.contains(&head.sequence) && crate::committed::digest(&at(head.sequence))? == head.digest);
    let ours = head.node == node || head.lease == 0;
    Ok(has && (ours || (!staged && versions.last().is_none_or(|&n| n == head.sequence))))
}

/// Drops this node's copy of `object` (under `objects`): its volume,
/// journal and marker, then its versions, which say whether a copy is
/// current, so a discard cut short is never taken for one.
fn discard(objects: &Path, object: &str) -> io::Result<()> {
    let [journal, staging, marker, versions] = ["~journal", "~restore", "~shipped", "~committed"].map(|d| objects.join(d).join(object));
    for path in [objects.join(object), journal, staging, versions] {
        if let Err(e) = std::fs::remove_dir_all(&path)
            && e.kind() != ErrorKind::NotFound
        {
            return Err(e);
        }
    }
    match std::fs::remove_file(marker) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
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

/// Rebuilds object `object` of app `app` here (under `objects`) as version
/// `to` of it in `store`, or as its head's: fetched back from the head,
/// each version the parent the one after it names and checked against its
/// digest, to `to` and on to the last snapshot before it. The head's is
/// kept so history goes on from it. An older version is restored as a new
/// one after the head, under lease epoch `epoch`, so the head only moves on
/// (once it is shipped) and the versions after the one restored stay in the
/// store. The object's volume, journal and versions here are replaced by
/// `install`. A crash before the restore is decided leaves the object as it
/// was; one after, `recover` finishes.
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
/// marked shipped, the journal of the volume they replace gone. Each step
/// can be taken again, so a crash in the middle is finished by `recover`.
fn install(objects: &Path, object: &str) -> io::Result<()> {
    let (app, stage) = (open_dir(objects)?, staging(objects, object));
    let at = open_dir(&stage)?;
    let staged = |name: &str| statat(&at, name, AtFlags::SYMLINK_NOFOLLOW).is_ok();
    #[cfg(test)]
    let cut = tests::cut;
    #[cfg(not(test))]
    let cut = || io::Result::Ok(());
    cut()?;
    remove_in(&app, "~journal", object)?;
    cut()?;
    if staged("volume") {
        remove_at(&app, object.as_ref())?;
        cut()?;
        renameat(&at, "volume", &app, object)?;
    }
    cut()?;
    if staged("history") {
        let first = published(&stage.join("history"))?.0.first().copied();
        let first = first.ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "a restore without versions"))?;
        mark(&marker(&objects.join("~committed").join(object)), first)?;
        cut()?;
        let committed = sub(&app, "~committed", owner(objects)?)?;
        remove_at(&committed, object.as_ref())?;
        cut()?;
        renameat(&at, "history", &committed, object)?;
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
    remove_in(&open_dir(objects)?, "~paused", object)
}

/// Whether object `id` (under `objects`, an app's) is paused for a restore.
pub fn paused(objects: &Path, id: &str) -> bool {
    objects.join("~paused").join(id).exists()
}

/// Syncs the filesystem `path` is on.
fn sync_fs(path: &Path) -> io::Result<()> {
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
        let dir = objects.join("~committed/o");
        let text = || std::fs::read_to_string(objects.join("o/v")).unwrap();
        for cut in 0.. {
            let _ = std::fs::remove_dir_all(&base);
            let mut parent = String::new();
            for n in 1..=3 {
                parent = version(&dir, n, 0, &parent, &format!("v{n}"));
                ship_object(&store, None, "app", "o", &dir).await.unwrap();
            }
            std::fs::create_dir_all(objects.join("o")).unwrap();
            std::fs::write(objects.join("o/v"), "v3").unwrap();
            std::fs::create_dir_all(objects.join("~journal/o")).unwrap();
            std::fs::write(objects.join("~journal/o/log"), "the old volume's").unwrap();
            pause(&objects, "o").unwrap();
            CUT.set(Some(cut));
            let restored = restore(&store, "app", "o", &objects, Some(2), 0).await;
            CUT.set(None);
            recover(&data).unwrap();
            assert!(!paused(&objects, "o") && !base.join("node/restore/app/o").exists(), "cut {cut}");
            let decided = restored.is_ok() || cut >= 2;
            let journal = objects.join("~journal/o/log").exists();
            let history = published(&dir).unwrap().0;
            match decided {
                false => assert_eq!((text(), journal, history), ("v3".into(), true, vec![3]), "cut {cut}"),
                true => assert_eq!((text(), journal, history), ("v2".into(), false, vec![3, 4]), "cut {cut}"),
            }
            ship_object(&store, None, "app", "o", &dir).await.unwrap();
            assert_eq!(head(&store, "app", "o").await.unwrap().0.sequence, 3 + u64::from(decided), "cut {cut}");
            if let Ok(restored) = restored {
                assert_eq!(restored, Restored { object: "o".into(), version: 2, head: 4 });
                assert_eq!(cut, 9, "cut points");
                break;
            }
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nanovm-ship-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
        std::fs::write(dir.join(format!("{n:020}.tar")), &bytes).unwrap();
        hex(&bytes)
    }

    /// What object `object` of `app` holds, restored from `store` on a fresh
    /// root: the text of its version, or why not.
    async fn restored(store: &dyn Store, object: &str, root: &Path) -> Result<String, String> {
        let _ = std::fs::remove_dir_all(root);
        std::fs::create_dir_all(root).unwrap();
        restore(store, "app", object, root, None, 0).await.map_err(|e| e.to_string())?;
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
        ship_object(&store, None, "app", "o", &dir).await.unwrap();
        let shipped = Head { sequence: 3, digest: parent.clone(), node: "".into(), lease: 0 };
        assert_eq!(head(&store, "app", "o").await.unwrap().0, shipped);
        assert_eq!((local(), std::fs::read_to_string(marker(&dir)).unwrap()), (vec![3], "3".into()));
        assert_eq!(restored(&store, "o", &base.join("fresh/objects/app")).await.unwrap(), "v3");
        // Another writer moves the head on.
        let (_, tag) = head(&store, "app", "o").await.unwrap();
        let theirs = Head { sequence: 4, digest: "0".repeat(64), ..shipped };
        store.swap("app/o/head", serde_json::to_vec(&theirs).unwrap(), tag.as_ref()).await.unwrap().unwrap();
        version(&dir, 4, 0, &parent, "v4");
        assert!(ship_object(&store, None, "app", "o", &dir).await.is_err());
        assert_eq!((local(), std::fs::read_to_string(marker(&dir)).unwrap()), (vec![3, 4], "3".into()));
        assert_eq!(head(&store, "app", "o").await.unwrap().0, theirs);
        std::fs::remove_dir_all(base).unwrap();
    }

    /// A version that names another app or object is never shipped as this
    /// one's, wherever it is found (a worker can put anything in its own
    /// directory).
    #[tokio::test]
    async fn a_version_naming_another_object_is_not_shipped() {
        let base = tempdir("identity");
        let store = Dir(base.join("store"));
        let (theirs, dir) = (base.join("objects/app/~committed/x"), base.join("objects/app/~committed/o"));
        version(&theirs, 1, 0, "", "x's");
        assert!(ship_object(&store, None, "other", "x", &theirs).await.is_err(), "another app's");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::rename(theirs.join(format!("{:020}.tar", 1)), dir.join(format!("{:020}.tar", 1))).unwrap();
        assert!(ship_object(&store, None, "app", "o", &dir).await.is_err(), "another object's");
        assert_eq!(head(&store, "app", "o").await.unwrap().0.sequence, 0);
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
        let (a_dir, b_dir) = (a_objects.join("~committed/o"), b_objects.join("~committed/o"));
        let ga = take(&store, &a, "o", &a_objects).await;
        version(&a_dir, 1, ga.epoch, "", "A's");
        let (shipped, gb) = tokio::join!(ship_object(&store, Some(&a), "app", "o", &a_dir), async {
            store.started.notified().await;
            let gb = take(&store, &b, "o", &b_objects).await;
            store.gate.add_permits(1);
            gb
        });
        assert!(shipped.is_err(), "A's version counts though B took the object at epoch {}", gb.epoch);
        assert!(!marker(&a_dir).exists(), "A marked its version shipped");
        // B serves the object, and its history is the object's.
        let digest = version(&b_dir, 1, gb.epoch, "", "B's");
        ship_object(&store, Some(&b), "app", "o", &b_dir).await.unwrap();
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
        ship_object(&store, Some(&a), "app", "o", &dir).await.unwrap();
        let two = commit(2, first.epoch, &one, "two");
        a.release("app/o", &first).await;
        // Taken again with no other node between: its copy stays, and
        // version 2 ships under the new lease.
        let again = take(&store, &a, "o", &a_objects).await;
        assert_eq!(text(&a_objects).as_deref(), Some("two"));
        ship_object(&store, Some(&a), "app", "o", &dir).await.unwrap();
        assert_eq!(head(&store, "app", "o").await.unwrap().0.sequence, 2);
        // A commits 3, but B takes the object before A ships it: B restores 2.
        commit(3, again.epoch, &two, "three");
        let theirs = take(&store, &b, "o", &b_objects).await;
        assert_eq!(text(&b_objects).as_deref(), Some("two"));
        b.release("app/o", &theirs).await;
        // A takes it back: its version 3 never was the object's, and goes.
        take(&store, &a, "o", &a_objects).await;
        assert_eq!((text(&a_objects).as_deref(), published(&dir).unwrap().0), (Some("two"), vec![2]));
        ship_object(&store, Some(&a), "app", "o", &dir).await.unwrap();
        assert_eq!(head(&store, "app", "o").await.unwrap().0.sequence, 2);
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
            let dir = objects.join("~committed").join(object);
            version(&dir, 1, epoch, "", "late");
            if ship_object(&store, Some(&a), "app", object, &dir).await.is_ok() || head(&store, "app", object).await.unwrap().0.sequence > 0 {
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
