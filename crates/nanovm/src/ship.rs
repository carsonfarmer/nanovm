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
    /// Removes everything under `prefix` (`<app>` or `<app>/<object>`).
    async fn remove(&self, prefix: &str) -> io::Result<()>;
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

    async fn remove(&self, prefix: &str) -> io::Result<()> {
        match tokio::fs::remove_dir_all(self.0.join(prefix)).await {
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            removed => removed,
        }
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
/// object whose versions are in `dir` (its directory's `committed`): its
/// directory's `shipped`.
pub(crate) fn marker(dir: &Path) -> PathBuf {
    dir.with_file_name("shipped")
}

/// Records `sequence` in `marker`, whole.
fn mark(marker: &Path, sequence: u64) -> io::Result<()> {
    let new = marker.with_file_name("shipped.new");
    std::fs::write(&new, sequence.to_string())?;
    std::fs::rename(new, marker)
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
                if n != head.sequence + 1 || version.parent != head.digest || version.epoch > lease {
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
/// until it does. It ships an object while it holds `turn`, which an
/// operator's delete takes, so neither sees the other half done.
pub async fn run(objects: PathBuf, store: Arc<dyn Store>, leases: Option<Arc<Leases>>, turn: Arc<tokio::sync::Mutex<()>>) {
    let mut seen = std::collections::HashMap::new();
    loop {
        let entries = |dir: PathBuf| std::fs::read_dir(dir).into_iter().flatten().flatten();
        let dirs = entries(objects.clone()).flat_map(|app| {
            let shards = entries(app.path()).filter(|s| s.file_name().len() == 2);
            shards.flat_map(move |s| entries(s.path())).map(move |o| (app.file_name(), o))
        });
        let mut failed = false;
        for (app, object) in dirs.collect::<Vec<_>>() {
            // Skip an object nothing was published for since its last round:
            // its directory's time is the same, and older than the clock's tick.
            let versions = object.path().join("committed");
            let Ok(changed) = versions.metadata().and_then(|m| m.modified()) else { continue };
            let old = changed.elapsed().is_ok_and(|e| e > Duration::from_millis(100));
            if old && seen.get(&versions) == Some(&changed) {
                continue;
            }
            let (app, id) = (app.to_string_lossy(), object.file_name().to_string_lossy().into_owned());
            let _turn = turn.lock().await;
            match ship_object(&*store, leases.as_deref(), &app, &id, &versions).await {
                Ok(()) => _ = seen.insert(versions, changed),
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

/// Claims the head of object `object` of app `app` for this node's lease on
/// it, at epoch `lease`, before the object is served here: by a conditional
/// write, after which no node with an earlier lease can move the head. This
/// node's copy of the object (under `objects`, `<data_dir>/objects/<app>`)
/// is then the head's: kept if it is (see `current`), else dropped and
/// restored from the store. So a node never serves state older than the
/// head, nor ships history the head has moved past.
pub async fn claim(store: &dyn Store, node: &str, lease: u64, app: &str, object: &str, objects: &Path) -> io::Result<()> {
    let dir = crate::committed::directory(&crate::app::object_dir(objects, object));
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
            restore(store, app, object, objects).await?;
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
    let base = crate::app::object_dir(objects, object);
    for name in [crate::journal::VOLUME, "journal", "restore", "shipped", "committed"] {
        let path = base.join(name);
        let removed = if name == "shipped" { std::fs::remove_file(path) } else { std::fs::remove_dir_all(path) };
        if let Err(e) = removed
            && e.kind() != ErrorKind::NotFound
        {
            return Err(e);
        }
    }
    Ok(())
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
    let base = crate::app::object_dir(objects, object);
    let staging = base.join("restore");
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
    let last = crate::committed::restore(&staging, &base.join(crate::journal::VOLUME))?;
    // The volume first, then the version that says it is there.
    sync_fs(objects)?;
    let out = crate::committed::directory(&base);
    std::fs::create_dir_all(&out)?;
    std::fs::rename(staging.join(format!("{last:020}.tar")), out.join(format!("{last:020}.tar")))?;
    mark(&marker(&out), last)?;
    std::fs::remove_dir_all(staging)?;
    sync_fs(objects)?;
    Ok(last)
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
        std::fs::write(dir.join(format!("{n:020}.tar")), &bytes).unwrap();
        hex(&bytes)
    }

    /// What object `object` of `app` holds, restored from `store` on a fresh
    /// root: the text of its version, or why not.
    async fn restored(store: &dyn Store, object: &str, root: &Path) -> Result<String, String> {
        let _ = std::fs::remove_dir_all(root);
        std::fs::create_dir_all(root).unwrap();
        restore(store, "app", object, root).await.map_err(|e| e.to_string())?;
        Ok(std::fs::read_to_string(crate::app::object_dir(root, object).join("data/v")).unwrap())
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
        let (store, dir) = (Dir(base.join("store")), crate::app::object_dir(&base.join("objects/app"), "o").join("committed"));
        let local = || published(&dir).unwrap().0;
        let mut parent = String::new();
        for n in 1..=3 {
            parent = version(&dir, n, 0, &parent, &format!("v{n}"));
        }
        ship_object(&store, None, "app", "o", &dir).await.unwrap();
        let shipped = Head { sequence: 3, digest: parent.clone(), node: "".into(), lease: 0 };
        assert_eq!(head(&store, "app", "o").await.unwrap().0, shipped);
        assert_eq!((local(), std::fs::read_to_string(marker(&dir)).unwrap()), (vec![3], "3".into()));
        assert_eq!(restored(&store, "o", &base.join("fresh")).await.unwrap(), "v3");
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
        let (a_dir, b_dir) = (crate::app::object_dir(&a_objects, "o").join("committed"), crate::app::object_dir(&b_objects, "o").join("committed"));
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
        let dir = crate::app::object_dir(&a_objects, "o").join("committed");
        let text = |objects: &Path| std::fs::read_to_string(crate::app::object_dir(objects, "o").join("data/v")).ok();
        // A version and the volume as it leaves it.
        let commit = |n: u64, epoch: u64, parent: &str, v: &str| {
            std::fs::create_dir_all(dir.with_file_name("data")).unwrap();
            std::fs::write(dir.with_file_name("data/v"), v).unwrap();
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
        let objects = base.join("a");
        // A has a version of `object`, committed at `epoch`; it must not count.
        let mut shipped = vec![];
        let mut refused = async |object: &str, epoch: u64| {
            let dir = crate::app::object_dir(&objects, object).join("committed");
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
