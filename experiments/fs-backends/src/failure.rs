//! Real-file I/O failures at each backend call, including partially completed writes.
use super::{Backend, Limits, Volume, payload};
use anyhow::{Result, ensure};
use redb::{BackendError, Database, StorageBackend, backends::FileBackend};
use std::{
    fs::{self, File},
    io,
    ops::Bound,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
    },
};

#[derive(Debug)]
struct Fault {
    remaining: AtomicUsize,
    fired: AtomicBool,
}

impl Fault {
    fn trip(&self) -> bool {
        self.remaining
            .try_update(SeqCst, SeqCst, |n| n.checked_sub(1))
            .is_ok_and(|n| {
                if n == 1 {
                    self.fired.store(true, SeqCst);
                    true
                } else {
                    false
                }
            })
    }
}

#[derive(Debug)]
struct FailingFile {
    file: FileBackend,
    fault: Arc<Fault>,
}

fn failure() -> io::Error {
    io::Error::other("injected storage failure")
}

impl StorageBackend for FailingFile {
    fn len(&self) -> io::Result<u64> {
        self.file.len()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        if self.fault.trip() {
            return Err(failure());
        }
        self.file.read(offset, out)
    }
    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        if self.fault.trip() {
            self.file.write(offset, &data[..data.len() / 2])?;
            return Err(failure());
        }
        self.file.write(offset, data)
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        if self.fault.trip() {
            return Err(failure());
        }
        self.file.set_len(len)
    }
    fn sync_data(&self) -> io::Result<()> {
        if self.fault.trip() {
            return Err(failure());
        }
        self.file.sync_data()
    }
    fn close(&self) -> io::Result<()> {
        self.file.close()
    }
    fn try_lock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<bool, BackendError> {
        self.file.try_lock_range(start, end)
    }
    fn try_lock_shared_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<bool, BackendError> {
        self.file.try_lock_shared_range(start, end)
    }
    fn lock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<(), BackendError> {
        self.file.lock_range(start, end)
    }
    fn lock_shared_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<(), BackendError> {
        self.file.lock_shared_range(start, end)
    }
    fn unlock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<(), BackendError> {
        self.file.unlock_range(start, end)
    }
    fn query_lock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<bool, BackendError> {
        self.file.query_lock_range(start, end)
    }
}

fn mutate(v: &mut Volume) -> Result<()> {
    let h = v.create("stage")?;
    v.write(h, 0, &payload(65539, 22))?;
    v.rename("stage", "a")?;
    v.unlink("b")?;
    let h = v.create("c")?;
    v.write(h, 0, b"new-c")?;
    Ok(())
}

fn contents(v: &mut Volume, name: &str) -> Result<Option<Vec<u8>>> {
    if v.named(name)?.is_none() {
        return Ok(None);
    }
    let h = v.open_file(name)?;
    let bytes = v.read(h)?;
    v.close(h)?;
    Ok(Some(bytes))
}

fn verify(v: &mut Volume, old_only: bool) -> Result<bool> {
    let a = contents(v, "a")?;
    let b = contents(v, "b")?;
    let c = contents(v, "c")?;
    let old = a == Some(payload(8193, 11)) && b.as_deref() == Some(b"old-b") && c.is_none();
    let new = a == Some(payload(65539, 22)) && b.is_none() && c.as_deref() == Some(b"new-c");
    ensure!(old || (!old_only && new), "torn state after storage failure");
    ensure!(v.named("stage")?.is_none() && v.files == 2, "leaked inode");
    ensure!(v.bytes == if old { 8198 } else { 65544 }, "incorrect recovered quota");
    v.backend.audit()?;
    Ok(new)
}

fn matrix(kind: &str, root: &Path, recovering: bool) -> Result<()> {
    let limits = Limits {
        bytes: 4 << 20,
        files: 8,
        handles: 8,
        cache: 4096,
    };
    let seed = root.join("seed");
    let mut v = Volume::open(kind, &seed, limits)?;
    let a = v.create("a")?;
    v.write(a, 0, &payload(8193, 11))?;
    let b = v.create("b")?;
    v.write(b, 0, b"old-b")?;
    if recovering {
        let orphan = v.create("orphan")?;
        // Shared content must survive collection of the disconnected inode.
        v.write(orphan, 0, &payload(8193, 11))?;
        v.unlink("orphan")?;
    }
    v.finish(true)?;
    drop(v);
    for cut in 1..=1024 {
        let trial = root.join(cut.to_string());
        fs::create_dir_all(&trial)?;
        fs::copy(seed.join("store.redb"), trial.join("store.redb"))?;
        let fault = Arc::new(Fault {
            remaining: AtomicUsize::new(0),
            fired: AtomicBool::new(false),
        });
        let file = File::options().read(true).write(true).open(trial.join("store.redb"))?;
        let db = Database::builder()
            .set_cache_size(limits.cache)
            .create_with_backend(FailingFile {
                file: FileBackend::new(file)?,
                fault: fault.clone(),
            })?;
        let backend = Backend::pages(db, kind == "hashed")?;
        if recovering {
            fault.remaining.store(cut, SeqCst);
        }
        let (changed, accepted) = match Volume::from_backend(backend, limits) {
            Ok(mut v) => {
                let changed = if recovering {
                    Ok(())
                } else {
                    let _held = v.open_file("a")?;
                    fault.remaining.store(cut, SeqCst);
                    mutate(&mut v)
                };
                // Isolate the actual commit result from starting the next transaction.
                let accepted = v.backend.end(true);
                fault.remaining.store(0, SeqCst);
                (changed, accepted)
            }
            Err(error) => (Err(error), Err(anyhow::anyhow!("recovery prevented acceptance"))),
        };
        let failed_mutation = changed.is_err();
        let fired = fault.fired.load(SeqCst);
        if fired {
            ensure!(
                accepted.is_err(),
                "caught storage failure was allowed to commit at cut {cut}"
            );
        } else {
            changed?;
            accepted?;
        }
        fault.remaining.store(0, SeqCst);
        let mut v = Volume::open(kind, &trial, limits)?;
        let old_only = recovering || failed_mutation;
        let new = verify(&mut v, old_only)?;
        ensure!(
            fired || new != recovering,
            "successful control lost its committed changes"
        );
        v.finish(true)?;
        drop(v);
        // Recovery itself must leave a stable result on a second open.
        let mut v = Volume::open(kind, &trial, limits)?;
        ensure!(
            verify(&mut v, old_only)? == new,
            "recovery changed generations on second reopen"
        );
        drop(v);
        fs::remove_dir_all(trial)?;
        if !fired {
            ensure!(cut > 1, "fault injector exercised no I/O");
            eprintln!(
                "PASS {kind} recovering={recovering}: {} I/O failure cuts, partial writes, caught-error commit refusal and reopen",
                cut - 1
            );
            return Ok(());
        }
    }
    anyhow::bail!("failure matrix did not reach the successful control")
}

#[test]
fn storage_errors_cannot_publish_partial_mutations() -> Result<()> {
    let root = std::env::current_dir()?.join(format!("target/fs-failures-{}", std::process::id()));
    ensure!(!root.exists(), "refuse existing failure-test directory");
    for kind in ["pages", "hashed"] {
        for recovering in [false, true] {
            matrix(kind, &root.join(format!("{kind}-{recovering}")), recovering)?;
        }
    }
    fs::remove_dir_all(root)?;
    Ok(())
}
