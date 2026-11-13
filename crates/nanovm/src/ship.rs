//! Ships each object's published versions to a store off the node, which
//! keeps each at `<app>/<object>/<sequence>.tar` once: a put never replaces
//! what is there, so a retry is harmless and a second writer of a sequence
//! is refused. With leases (W-10), a node ships an object's version only
//! while the lease record names it, or still has the version's epoch: a node
//! that lost the object can't ship what it committed late (W-90). Shipped
//! versions are removed here but the last, which the next names as its
//! parent; what isn't shipped yet stays, within the capture limits, which
//! refuse writes once they are reached (the store down, say), never
//! dropping a version.

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use object_store::{ObjectStore, ObjectStoreExt, PutMode};

use crate::config::CaptureConfig;
use crate::lease::Leases;

pub enum Put {
    Stored,
    /// The same bytes were there already.
    Same,
    /// Other bytes were.
    Conflict,
}

/// Immutable keys: a put stores its bytes only where nothing is.
#[async_trait::async_trait]
pub trait Store: Send + Sync {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> io::Result<Put>;
    async fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>>;
}

/// The store `cap` names, if any: a directory (`/path`), or an object store
/// URL as `leases` takes.
pub fn store(cap: &CaptureConfig) -> anyhow::Result<Option<Arc<dyn Store>>> {
    Ok(match cap.store.as_deref() {
        None => None,
        Some(dir) if dir.starts_with('/') => Some(Arc::new(Dir(dir.into()))),
        Some(url) => Leases::open(url)?.map(|s| Arc::new(Objects(s)) as Arc<dyn Store>),
    })
}

/// A local (or mounted) directory: a put is a hard link, which never
/// replaces a name.
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
                Err(e) => Err(e),
                Ok(()) => {
                    // The name; and those of the directories it may have made.
                    let names = if fresh { dir.ancestors().position(|d| d == root).map_or(1, |n| n + 1) } else { 1 };
                    for d in dir.ancestors().take(names) {
                        std::fs::File::open(d)?.sync_all()?;
                    }
                    Ok(Put::Stored)
                }
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
}

/// An object store (S3 and the like): a put is conditional on nothing
/// being there.
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
        match self.0.get(&key.into()).await {
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            res => Ok(Some(res.map_err(io::Error::other)?.bytes().await.map_err(io::Error::other)?.into())),
        }
    }
}

fn key(app: &str, object: &str, sequence: u64) -> String {
    format!("{app}/{object}/{sequence:020}.tar")
}

/// Where the shipper records the last version it stored of the object whose
/// versions are in `dir` (`<objects>/~committed/<object>`):
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

/// Waits, two seconds at most (within an instance's post-return deadline),
/// for the shipper to store the version `sequence` of the object whose
/// versions are in `dir`.
pub(crate) fn stored(dir: &Path, sequence: u64) -> io::Result<()> {
    for _ in 0..1000 {
        if std::fs::read_to_string(marker(dir)).ok().and_then(|s| s.parse::<u64>().ok()) >= Some(sequence) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(io::Error::new(ErrorKind::TimedOut, format!("version {sequence} is not in the store")))
}

/// The sequences of the versions published in `dir`, in order.
fn published(dir: &Path) -> io::Result<Vec<u64>> {
    let mut versions = vec![];
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name();
        if let Some(n) = name.to_str().and_then(|n| n.strip_suffix(".tar")?.parse().ok()) {
            versions.push(n);
        }
    }
    versions.sort();
    Ok(versions)
}

/// Ships what the object whose versions are in `dir` hasn't yet, in order,
/// then removes the shipped ones but the last. With `leases`, only versions
/// the object's lease record allows: it names this node, or still has the
/// version's epoch.
pub(crate) async fn ship_object(store: &dyn Store, leases: Option<&Leases>, app: &str, object: &str, dir: &Path) -> io::Result<()> {
    let marker = marker(dir);
    let shipped: u64 = std::fs::read_to_string(&marker).ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let versions = published(dir)?;
    let mut holder = None;
    if let Some(leases) = leases.filter(|_| versions.iter().any(|&n| n > shipped)) {
        holder = leases.holder(&format!("{app}/{object}")).await.map_err(io::Error::other)?;
    }
    for &n in versions.iter().filter(|&&n| n > shipped) {
        let path = dir.join(format!("{n:020}.tar"));
        if let (Some(leases), Some((node, epoch))) = (leases, &holder)
            && *node != leases.node()
            && *epoch != crate::committed::epoch(&path)?
        {
            return Err(io::Error::other(format!("{app}/{object}'s lease moved to {node} at epoch {epoch}")));
        }
        match store.put(&key(app, object, n), tokio::fs::read(&path).await?).await? {
            Put::Stored | Put::Same => {}
            Put::Conflict => return Err(io::Error::other(format!("{app}/{object} version {n} is another writer's"))),
        }
        mark(&marker, n)?;
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
/// a second later.
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
                Err(e) => {
                    eprintln!("nanovm: shipping {app}/{id}: {e}");
                    failed = true;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(if failed { 1000 } else { 20 })).await;
    }
}

/// Rebuilds object `object` of app `app` from what `store` holds, as the
/// volume under `objects` (`<data_dir>/objects/<app>`, where it must not be
/// yet), with its last version kept so history goes on from it. Returns
/// that version's sequence.
pub async fn restore(store: &dyn Store, app: &str, object: &str, objects: &Path) -> io::Result<u64> {
    let staging = objects.join("~restore").join(object);
    std::fs::create_dir_all(&staging)?;
    let mut n = 1;
    while let Some(bytes) = store.get(&key(app, object, n)).await? {
        std::fs::write(staging.join(format!("{n:020}.tar")), bytes)?;
        n += 1;
    }
    let last = crate::committed::restore(&staging, &objects.join(object))?;
    let out = objects.join("~committed").join(object);
    std::fs::create_dir_all(&out)?;
    std::fs::rename(staging.join(format!("{last:020}.tar")), out.join(format!("{last:020}.tar")))?;
    mark(&marker(&out), last)?;
    std::fs::remove_dir_all(staging)?;
    // SAFETY: sync takes no arguments and cannot fail.
    unsafe { libc::sync() };
    Ok(last)
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

    /// With leases, a node that isn't the holder ships a version only while
    /// the lease record still has its epoch: after a release, say.
    #[tokio::test]
    async fn the_lease_records_epoch_decides_for_other_nodes() {
        let base = tempdir("released");
        let records: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let a = Leases::new(records.clone(), "a", Duration::from_secs(5));
        a.renew().await;
        let grant = a.acquire("app/p").await.unwrap();
        a.release("app/p", &grant).await;
        let (store, dir) = (Dir(base.join("store")), base.join("objects/app/~committed/p"));
        std::fs::create_dir_all(&dir).unwrap();
        let version = |n: u64, epoch: u64| {
            let id = format!(r#"{{"format":3,"app":"app","object":"p","sequence":{n},"epoch":{epoch},"parent":"","snapshot":true}}"#);
            let mut tar = tar::Builder::new(vec![]);
            let mut header = tar::Header::new_gnu();
            header.set_size(id.len() as u64);
            header.set_mode(0o600);
            tar.append_data(&mut header, "nanovm.json", id.as_bytes()).unwrap();
            std::fs::write(dir.join(format!("{n:020}.tar")), tar.into_inner().unwrap()).unwrap();
        };
        let b = Leases::new(records, "b", Duration::from_secs(5));
        version(1, grant.epoch);
        ship_object(&store, Some(&b), "app", "p", &dir).await.unwrap();
        version(2, grant.epoch + 1);
        assert!(ship_object(&store, Some(&b), "app", "p", &dir).await.is_err());
        assert!(store.get(&key("app", "p", 2)).await.unwrap().is_none());
        std::fs::remove_dir_all(base).unwrap();
    }

    /// The shipper stores versions in order, then keeps only the last here;
    /// it stops at a version another writer stored.
    #[tokio::test]
    async fn the_shipper_stores_in_order_and_keeps_the_last() {
        let base = tempdir("shipper");
        let (store, dir) = (Dir(base.join("store")), base.join("objects/app/~committed/o"));
        std::fs::create_dir_all(&dir).unwrap();
        let version = |n: u64| std::fs::write(dir.join(format!("{n:020}.tar")), format!("v{n}")).unwrap();
        let local = || published(&dir).unwrap();
        (1..=3).for_each(version);
        ship_object(&store, None, "app", "o", &dir).await.unwrap();
        assert_eq!(store.get(&key("app", "o", 2)).await.unwrap().unwrap(), b"v2");
        assert_eq!((local(), std::fs::read_to_string(marker(&dir)).unwrap()), (vec![3], "3".into()));
        version(4);
        store.put(&key("app", "o", 5), b"theirs".to_vec()).await.unwrap();
        version(5);
        assert!(ship_object(&store, None, "app", "o", &dir).await.is_err());
        assert_eq!((local(), std::fs::read_to_string(marker(&dir)).unwrap()), (vec![3, 4, 5], "4".into()));
        std::fs::remove_dir_all(base).unwrap();
    }
}
