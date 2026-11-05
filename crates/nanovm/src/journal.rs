//! A request's changes to its object's volume take effect together or not at
//! all, and survive a crash once taken: an undo journal, as SQLite keeps.
//!
//! Before each change, what it would destroy is recorded and synced:
//! - a file's size, the first time it is written or truncated;
//! - each original byte a write or truncation overwrites, once;
//! - "remove this", for a new file or directory;
//! - "rename it back", for a rename; a removed or replaced file is itself
//!   renamed into the journal if it existed before the request;
//! - "recreate this", for a removed directory.
//!
//! Writes then happen in place, so open files stay valid. Committing syncs
//! what changed and deletes the log. Rolling back replays it backwards, which
//! is what [`recover`] does when an instance starts, so a failed request (its
//! instance is restarted) and a crash take the same path. Recovery drops each
//! record once undone, so a crash during it resumes where it stopped.
//!
//! The log lives beside the volume, at `<objects>/~journal/<object>`, which
//! no object id can name and no guest path reaches. Paths in it are relative
//! to `<objects>`: in the volume, or in the journal for a file the guest
//! still writes after removing it. The log, and the removed files, may each
//! hold as much as the object's quota.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Instant;

use crate::object::ObjShared;

const LOG: &str = "log";
/// Overwritten bytes are kept in records of at most this many.
const CHUNK: u64 = 1 << 20;

enum Undo {
    Size(PathBuf, u64),
    Bytes(PathBuf, u64, Vec<u8>),
    Created(PathBuf),
    RemovedDir(PathBuf),
    /// The entry at the first path was renamed to the second.
    Renamed(PathBuf, PathBuf),
    /// A rename that failed, durably cancelled before paths can be reused.
    Cancelled,
}

struct Saved {
    len: u64,
    /// Sorted, disjoint ranges whose request-start bytes are durable.
    ranges: Vec<std::ops::Range<u64>>,
}

pub struct Journal {
    /// `<objects>`, which every path in the log is relative to.
    base: PathBuf,
    volume: PathBuf,
    dir: PathBuf,
    /// Open while the current request has changes.
    log: Option<File>,
    /// Files (by inode) whose size is recorded, and what to sync on commit.
    sized: HashMap<u64, Saved>,
    /// Successful creations, with every guest open of that inode. Weak
    /// handles include stream ownership without keeping the file alive.
    created: HashMap<u64, Vec<Weak<File>>>,
    /// Unlinked new files still held open: their removal-time bytes remain
    /// charged to the retained-file budget until the last handle closes.
    unlinked: Vec<(Vec<Weak<File>>, u64)>,
    touched: HashSet<PathBuf>,
    slots: u64,
    /// Bytes the log holds, and the removed files; each may reach `max`.
    held: (u64, u64),
    max: u64,
    poisoned: bool,
    /// Waits for the disk aren't charged to its CPU budget.
    obj: Arc<ObjShared>,
}

impl Journal {
    /// The journal of `volume` (an absolute path without symlinks), whose
    /// log and removed files may each hold `max` bytes, after rolling back
    /// what a previous instance left uncommitted.
    pub fn open(volume: &Path, max: u64, obj: Arc<ObjShared>) -> std::io::Result<Journal> {
        let base = volume.parent().expect("a volume has a parent").to_path_buf();
        let dir = base.join("~journal").join(volume.file_name().expect("a volume has a name"));
        recover(&base, &dir)?;
        Ok(Journal {
            base,
            volume: volume.to_path_buf(),
            dir,
            log: None,
            sized: HashMap::new(),
            created: HashMap::new(),
            unlinked: vec![],
            touched: HashSet::new(),
            slots: 0,
            held: (0, 0),
            max,
            poisoned: false,
            obj,
        })
    }

    /// The guest path `path`, if it is in the volume, without `..`. Nothing
    /// there is a symlink, so resolving `..` by hand is exact.
    pub fn local(&self, path: &Path) -> Option<PathBuf> {
        let mut out = PathBuf::from("/");
        for c in path.components() {
            match c {
                Component::ParentDir => _ = out.pop(),
                Component::Normal(n) => out.push(n),
                _ => {}
            }
        }
        out.starts_with(&self.volume).then_some(out)
    }

    /// The guest path `path` under the directory `dir`, if in the volume.
    pub fn local_at(&self, dir: &File, path: &str) -> Option<PathBuf> {
        (dir.metadata().ok()?.nlink() > 0).then_some(())?;
        self.local(&fd_path(dir)?.join(path))
    }

    /// Where `file` is, relative to the base, if in the volume or (removed
    /// while open) the journal.
    fn local_file(&self, file: &File) -> Option<PathBuf> {
        if file.metadata().ok()?.nlink() == 0 {
            return None;
        }
        let p = fd_path(file)?;
        (p.starts_with(&self.volume) || p.starts_with(&self.dir)).then(|| self.rel(&p))
    }

    /// Whether `file` was removed into the journal or unlinked, so its
    /// removal has already credited the volume's quota.
    pub fn holds(&self, file: &File) -> bool {
        file.metadata().is_ok_and(|m| m.nlink() == 0) || fd_path(file).is_some_and(|p| p.starts_with(&self.dir))
    }

    fn rel(&self, abs: &Path) -> PathBuf {
        abs.strip_prefix(&self.base).expect("inside the base").to_path_buf()
    }

    /// Counts `log` bytes more logged and `removed` more removed, or refuses
    /// as a full disk.
    fn charge(&mut self, log: u64, removed: u64) -> std::io::Result<()> {
        self.unlinked.retain(|(handles, bytes)| {
            if handles.iter().any(|h| h.strong_count() > 0) {
                true
            } else {
                self.held.1 -= bytes;
                false
            }
        });
        let held = (self.held.0 + log, self.held.1 + removed);
        if held.0 > self.max || held.1 > self.max {
            return Err(ErrorKind::StorageFull.into());
        }
        self.held = held;
        Ok(())
    }

    /// Appends `undo` and syncs it, before the change it undoes is made.
    fn record(&mut self, undo: Undo) -> std::io::Result<()> {
        self.check()?;
        let rec = encode(&undo);
        self.charge(rec.len() as u64, 0)?;
        // An I/O failure may leave a complete or partial record. Do not
        // permit more mutations or a commit until recovery has run.
        self.poisoned = true;
        let t0 = Instant::now();
        if self.log.is_none() {
            // The directories the log is first made in must be durable too.
            std::fs::create_dir_all(&self.dir)?;
            let log = File::options().create(true).append(true).open(self.dir.join(LOG))?;
            for d in [&self.dir, self.dir.parent().expect("in ~journal"), &self.base] {
                sync(d)?;
            }
            self.log = Some(log);
        }
        let log = self.log.as_mut().expect("opened above");
        log.write_all(&rec)?;
        #[cfg(test)]
        tests::sync_point()?;
        log.sync_data()?;
        self.poisoned = false;
        self.obj.uncharge(t0.elapsed());
        Ok(())
    }

    /// Before `file` is written over `[at, end)`, if it is in the volume:
    /// its original size and any original bytes not already saved. A new
    /// file needs neither: its `Created` record removes it on rollback.
    pub fn writing(&mut self, file: &File, at: u64, end: u64) -> std::io::Result<()> {
        let Some(p) = self.local_file(file) else { return Ok(()) };
        let md = file.metadata()?;
        if self.created.contains_key(&md.ino()) {
            return Ok(());
        }
        if !self.sized.contains_key(&md.ino()) {
            self.record(Undo::Size(p.clone(), md.len()))?;
            self.sized.insert(md.ino(), Saved { len: md.len(), ranges: vec![] });
            self.touched.insert(p.clone());
        }
        // The parts of `[at, end)` within the original size not saved yet.
        let saved = &self.sized[&md.ino()];
        let end = end.min(saved.len);
        let (mut gaps, mut from) = (vec![], at);
        for r in saved.ranges.iter().filter(|r| r.end > at && r.start < end) {
            if r.start > from {
                gaps.push(from..r.start);
            }
            from = r.end;
        }
        if from < end {
            gaps.push(from..end);
        }
        if gaps.is_empty() {
            return Ok(());
        }
        // Read through a handle of our own: the guest's may be write-only.
        let current = File::open(self.base.join(&p))?;
        for gap in gaps {
            for at in gap.clone().step_by(CHUNK as usize) {
                let mut old = vec![0; (gap.end - at).min(CHUNK) as usize];
                current.read_exact_at(&mut old, at)?;
                let n = old.len() as u64;
                self.record(Undo::Bytes(p.clone(), at, old))?;
                // Only a durable record protects a range, including when a
                // later chunk fails and the caller retries the whole write.
                save(&mut self.sized.get_mut(&md.ino()).expect("sized above").ranges, at..at + n);
            }
        }
        Ok(())
    }

    /// Before the entry at `path` is created, if nothing is there yet.
    pub fn creating(&mut self, path: &Path) -> std::io::Result<()> {
        let path: PathBuf = path.components().collect();
        if std::fs::symlink_metadata(&path).is_ok() {
            return Ok(());
        }
        let p = self.rel(&path);
        self.record(Undo::Created(p.clone()))?;
        self.touched.insert(p.clone());
        self.touch_parent(&p);
        Ok(())
    }

    /// After a file was successfully created under a durable `Created`
    /// record. Callers must not use this for an open of an existing file.
    pub fn created(&mut self, path: &Path) -> std::io::Result<()> {
        self.created.insert(std::fs::metadata(path)?.ino(), vec![]);
        Ok(())
    }

    /// Every successful guest open, including read-only opens. Streams
    /// clone this same Arc, so their lifetimes are covered as well.
    pub fn opened(&mut self, file: &Arc<File>) -> std::io::Result<()> {
        if let Some(handles) = self.created.get_mut(&file.metadata()?.ino()) {
            handles.retain(|h| h.strong_count() > 0);
            handles.push(Arc::downgrade(file));
        }
        Ok(())
    }

    /// Unlinks a file. Only files present before the request need a slot
    /// for recovery; new files with live descriptors still pay for storage.
    pub fn unlink(&mut self, path: &Path, bytes: u64) -> std::io::Result<()> {
        let ino = std::fs::metadata(path)?.ino();
        if self.created.get(&ino).is_some_and(|hs| hs.iter().all(|h| h.strong_count() == 0)) {
            std::fs::remove_file(path)?;
            self.created.remove(&ino);
            self.touch_parent(&self.rel(path));
            return Ok(());
        }
        let slot = self.remove(path, bytes)?;
        self.discard_created(&slot, bytes)
    }

    /// After a staged removal can no longer need restoring on syscall
    /// failure (notably rename), discard its data if rollback never needs it.
    pub fn discard_created(&mut self, slot: &Path, bytes: u64) -> std::io::Result<()> {
        let ino = std::fs::metadata(slot)?.ino();
        if self.created.contains_key(&ino) {
            std::fs::remove_file(slot)?;
            // Forget identity before the inode can be reused. No size or
            // byte records are ever made for these files.
            let handles = self.created.remove(&ino).unwrap();
            if handles.iter().any(|h| h.strong_count() > 0) {
                self.unlinked.push((handles, bytes));
            } else {
                self.held.1 -= bytes;
            }
        }
        Ok(())
    }

    /// Before the empty directory at `path` is removed, if one is there.
    pub fn removing_dir(&mut self, path: &Path) -> std::io::Result<()> {
        if !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
            return Ok(());
        }
        let p = self.rel(path);
        self.record(Undo::RemovedDir(p.clone()))?;
        self.touch_parent(&p);
        Ok(())
    }

    /// Removes the file at `path`, of `bytes`, by moving it into the
    /// journal: where it now is.
    pub fn remove(&mut self, path: &Path, bytes: u64) -> std::io::Result<PathBuf> {
        self.charge(0, bytes)?;
        self.slots += 1;
        let slot = self.dir.join(self.slots.to_string());
        let p = self.rel(path);
        let result = self.record(Undo::Renamed(p.clone(), self.rel(&slot)))
            .and_then(|()| std::fs::rename(path, &slot));
        if let Err(e) = result {
            self.held.1 -= bytes;
            return Err(e);
        }
        self.touch_parent(&p);
        Ok(slot)
    }

    /// Before `from` is renamed to `to`, where nothing is or an empty
    /// directory (removed with [`Self::removing_dir`]) is.
    pub fn renaming(&mut self, from: &Path, to: &Path) -> std::io::Result<u64> {
        let (from, to) = (self.rel(from), self.rel(to));
        let at = self.held.0;
        self.record(Undo::Renamed(from.clone(), to.clone()))?;
        Ok(at)
    }

    /// Only a successful syscall moves the dirty paths to their new names.
    pub fn renamed(&mut self, from: &Path, to: &Path) {
        let (from, to) = (self.rel(from), self.rel(to));
        // What is to be synced moves with it.
        let moved = |p: PathBuf| match p.strip_prefix(&from) {
            Ok(rest) if rest.as_os_str().is_empty() => to.clone(),
            Ok(rest) => to.join(rest),
            Err(_) => p,
        };
        self.touched = self.touched.drain().map(moved).collect();
        self.touch_parent(&from);
        self.touch_parent(&to);
    }

    /// Restore a staged target before cancelling the attempted rename.
    /// Sync both directories before invalidating any undo record: a crash
    /// must see either the restored target or the undo that restores it.
    pub fn restore(&mut self, slot: &Path, to: &Path, bytes: u64) -> std::io::Result<()> {
        self.or_poison(|j| {
            std::fs::rename(slot, to)?;
            sync(to.parent().expect("volume entry"))?;
            sync(&j.dir)?;
            j.held.1 -= bytes;
            Ok(())
        })
    }

    /// The failed rename's record must not act on a later reuse of either
    /// name. Replace only its tag, preserving framing and other concurrent
    /// operations' records. Cancelled records still consume log budget.
    pub fn cancel_rename(&mut self, at: u64) -> std::io::Result<()> {
        self.or_poison(|j| {
            let log = File::options().write(true).open(j.dir.join(LOG))?;
            log.write_all_at(&[5], at + 8)?;
            #[cfg(test)]
            tests::sync_point()?;
            log.sync_data()
        })
    }

    /// Runs `f`, whose waits for the disk aren't charged as CPU; if it
    /// fails, the journal needs recovery.
    fn or_poison(&mut self, f: impl FnOnce(&mut Self) -> std::io::Result<()>) -> std::io::Result<()> {
        let t0 = Instant::now();
        let result = f(self);
        self.obj.uncharge(t0.elapsed());
        self.poisoned |= result.is_err();
        result
    }

    pub fn check(&self) -> std::io::Result<()> {
        if self.poisoned {
            return Err(std::io::Error::other("journal requires recovery"));
        }
        Ok(())
    }

    fn touch_parent(&mut self, p: &Path) {
        self.touched.insert(p.parent().unwrap_or(Path::new("")).to_path_buf());
    }

    /// Makes the request's changes durable and forgets how to undo them.
    pub fn commit(&mut self) -> std::io::Result<()> {
        self.check()?;
        let Some(log) = self.log.take() else { return Ok(()) };
        // Any error leaves the outcome uncertain. In particular, taking the
        // log must not make a second commit look like an empty successful one.
        self.poisoned = true;
        let t0 = Instant::now();
        for p in self.touched.drain() {
            sync_if_there(&self.base.join(p))?;
        }
        drop(log);
        // Deleting the log is the commit.
        std::fs::remove_file(self.dir.join(LOG))?;
        sync(&self.dir)?;
        self.obj.uncharge(t0.elapsed());
        clear(&self.dir)?;
        self.sized.clear();
        self.created.clear();
        self.held = (0, self.unlinked.iter().map(|(_, bytes)| bytes).sum());
        self.poisoned = false;
        Ok(())
    }
}

/// Rolls back the log in `dir` if there is one, then empties `dir`. Failure
/// leaves the current undo intact and refuses to start the object. Only
/// already-applied or never-performed operations may be skipped.
fn recover(base: &Path, dir: &Path) -> std::io::Result<()> {
    let log = match std::fs::read(dir.join(LOG)) {
        Ok(log) => log,
        Err(e) if e.kind() == ErrorKind::NotFound => return clear(dir),
        Err(e) => return Err(e),
    };
    // Each record is dropped from the log once undone and synced, so a crash
    // here resumes where it stopped; the undo it was in may run twice, which
    // each tolerates. A record cut short by a crash was never acted on.
    let file = File::options().write(true).open(dir.join(LOG))?;
    for (at, undo) in decode(&log).into_iter().rev() {
        let abs = |p: &PathBuf| base.join(p);
        let open = |p: &PathBuf| File::options().write(true).open(abs(p));
        match &undo {
            Undo::Size(p, n) => open(p).and_then(|f| f.set_len(*n)),
            Undo::Bytes(p, at, data) => open(p).and_then(|f| f.write_all_at(data, *at)),
            Undo::Created(p) => match std::fs::symlink_metadata(abs(p)) {
                Ok(md) if md.is_dir() => std::fs::remove_dir(abs(p)),
                Ok(_) => std::fs::remove_file(abs(p)),
                Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            },
            Undo::RemovedDir(p) => std::fs::create_dir(abs(p)).or_else(|e| {
                if e.kind() == ErrorKind::AlreadyExists && std::fs::symlink_metadata(abs(p))?.is_dir() {
                    Ok(())
                } else {
                    Err(e)
                }
            }),
            Undo::Renamed(from, to) => {
                // The source is present after an earlier replay or a failed
                // syscall. Both may be absent for a discarded new file.
                if abs(from).try_exists()? || !abs(to).try_exists()? {
                    Ok(())
                } else {
                    std::fs::rename(abs(to), abs(from))
                }
            }
            Undo::Cancelled => Ok(()),
        }?;
        let p = match &undo {
            Undo::Size(p, _) | Undo::Bytes(p, ..) => p.as_path(),
            Undo::Created(p) | Undo::RemovedDir(p) | Undo::Renamed(p, _) => p.parent().unwrap_or(Path::new("")),
            Undo::Cancelled => Path::new(""),
        };
        sync_if_there(&base.join(p))?;
        if let Undo::Renamed(_, to) = &undo {
            // A cross-directory rename changes both parents. Persist both
            // before dropping its undo, including when replay runs twice.
            sync_if_there(&base.join(to.parent().unwrap_or(Path::new(""))))?;
        }
        #[cfg(test)]
        tests::crash_point()?;
        file.set_len(at)?;
        #[cfg(test)]
        tests::sync_point()?;
        file.sync_data()?;
    }
    std::fs::remove_file(dir.join(LOG))?;
    sync(dir)?;
    clear(dir)
}

/// A record: a tag, then its fields; a path or bytes as a length and the
/// bytes, a number as 8 bytes.
fn encode(undo: &Undo) -> Vec<u8> {
    let mut buf = vec![];
    let mut put = |b: &[u8]| {
        buf.extend((b.len() as u64).to_le_bytes());
        buf.extend(b);
    };
    let p = |p: &PathBuf| p.as_os_str().as_encoded_bytes().to_vec();
    let (tag, fields): (u8, Vec<Vec<u8>>) = match undo {
        Undo::Size(a, n) => (0, vec![p(a), n.to_le_bytes().to_vec()]),
        Undo::Bytes(a, at, data) => (1, vec![p(a), at.to_le_bytes().to_vec(), data.clone()]),
        Undo::Created(a) => (2, vec![p(a)]),
        Undo::RemovedDir(a) => (3, vec![p(a)]),
        Undo::Renamed(a, b) => (4, vec![p(a), p(b)]),
        Undo::Cancelled => unreachable!("only made by cancelling a rename's tag"),
    };
    put(&[tag]);
    fields.iter().for_each(|f| put(f));
    buf
}

/// The records in `log`, with where each starts, up to the first incomplete
/// one.
fn decode(mut log: &[u8]) -> Vec<(u64, Undo)> {
    let len = log.len();
    fn bytes(log: &mut &[u8]) -> Option<Vec<u8>> {
        let (n, rest) = log.split_at_checked(8)?;
        let (b, rest) = rest.split_at_checked(usize::try_from(u64::from_le_bytes(n.try_into().ok()?)).ok()?)?;
        *log = rest;
        Some(b.to_vec())
    }
    fn num(log: &mut &[u8]) -> Option<u64> {
        Some(u64::from_le_bytes(bytes(log)?.try_into().ok()?))
    }
    fn path(log: &mut &[u8]) -> Option<PathBuf> {
        // SAFETY: written from an `OsStr`'s encoded bytes.
        Some(unsafe { std::ffi::OsString::from_encoded_bytes_unchecked(bytes(log)?) }.into())
    }
    let next = |log: &mut &[u8]| -> Option<Undo> {
        Some(match bytes(log)?[..] {
            [0] => Undo::Size(path(log)?, num(log)?),
            [1] => Undo::Bytes(path(log)?, num(log)?, bytes(log)?),
            [2] => Undo::Created(path(log)?),
            [3] => Undo::RemovedDir(path(log)?),
            [4] => Undo::Renamed(path(log)?, path(log)?),
            [5] => {
                path(log)?;
                path(log)?;
                Undo::Cancelled
            }
            _ => return None,
        })
    };
    std::iter::from_fn(|| {
        let at = (len - log.len()) as u64;
        Some((at, next(&mut log)?))
    })
    .collect()
}

/// Adds `r`, disjoint from the sorted, disjoint `ranges`, merged with the
/// ranges it touches.
fn save(ranges: &mut Vec<std::ops::Range<u64>>, r: std::ops::Range<u64>) {
    let i = ranges.partition_point(|x| x.start < r.start);
    ranges.insert(i, r);
    if i + 1 < ranges.len() && ranges[i].end == ranges[i + 1].start {
        ranges[i].end = ranges.remove(i + 1).end;
    }
    if i > 0 && ranges[i - 1].end == ranges[i].start {
        ranges[i - 1].end = ranges.remove(i).end;
    }
}

/// Removes everything in `dir` (slots left by a commit or a rollback).
fn clear(dir: &Path) -> std::io::Result<()> {
    match std::fs::read_dir(dir) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(mut entries) => entries.try_for_each(|entry| {
            let e = entry?;
            let p = e.path();
            if e.file_type()?.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) }
        }),
    }
}

/// Syncs a file or a directory.
fn sync(p: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    tests::sync_point()?;
    File::open(p)?.sync_all()
}

/// Syncs `p` unless a later change removed it.
fn sync_if_there(p: &Path) -> std::io::Result<()> {
    match sync(p) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// Where the file behind `file` is now.
fn fd_path(file: &File) -> Option<PathBuf> {
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "linux")]
    return std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok();
    #[cfg(target_os = "macos")]
    {
        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        // SAFETY: F_GETPATH writes at most PATH_MAX bytes into `buf`.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } < 0 {
            return None;
        }
        let n = buf.iter().position(|&b| b == 0)?;
        buf.truncate(n);
        Some(PathBuf::from(std::ffi::OsString::from(String::from_utf8(buf).ok()?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A volume `<tmp>/<n>/o` holding `a`, `d/b`, an empty `e` and `f`.
    fn volume() -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("nanovm-journal-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let v = base.join("o");
        std::fs::create_dir_all(v.join("d")).unwrap();
        std::fs::create_dir_all(v.join("e")).unwrap();
        std::fs::write(v.join("a"), "hello world").unwrap();
        std::fs::write(v.join("d/b"), "bee").unwrap();
        std::fs::write(v.join("f"), "ff").unwrap();
        v.canonicalize().unwrap()
    }

    /// Every entry under `v`: a file's contents, or `None` for a directory.
    fn tree(v: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![v.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p.clone());
                    out.insert(p.strip_prefix(v).unwrap().to_path_buf(), None);
                } else {
                    out.insert(p.strip_prefix(v).unwrap().to_path_buf(), Some(std::fs::read(&p).unwrap()));
                }
            }
        }
        out
    }

    fn reopen(v: &Path, max: u64) -> std::io::Result<Journal> {
        Journal::open(v, max, Arc::new(ObjShared::new("app".into(), "o")))
    }

    fn open(v: &Path) -> Journal {
        reopen(v, 1 << 30).unwrap()
    }

    /// Writes as the filesystem host does: journal first, then the write.
    fn write(j: &mut Journal, v: &Path, name: &str, at: u64, data: &[u8]) {
        let file = File::options().read(true).write(true).open(v.join(name)).unwrap();
        j.writing(&file, at, at + data.len() as u64).unwrap();
        file.write_all_at(data, at).unwrap();
    }

    /// Renames as the filesystem host does: a file in the way goes into the
    /// journal, then the rename is recorded and made.
    fn rename(j: &mut Journal, v: &Path, from: &str, to: &str) {
        let (from, to) = (v.join(from), v.join(to));
        let replaced = std::fs::metadata(&to).ok().map(|md| (j.remove(&to, md.len()).unwrap(), md.len()));
        j.renaming(&from, &to).unwrap();
        std::fs::rename(&from, &to).unwrap();
        j.renamed(&from, &to);
        if let Some((slot, bytes)) = replaced {
            j.discard_created(&slot, bytes).unwrap();
        }
    }

    thread_local! {
        /// Undos recovery may apply before a simulated crash.
        static CRASH_AFTER: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
        static FAIL_SYNC_AFTER: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    }

    pub(super) fn sync_point() -> std::io::Result<()> {
        FAIL_SYNC_AFTER.with(|c| match c.get() {
            Some(0) => Err(std::io::Error::from_raw_os_error(libc::EIO)),
            n => Ok(c.set(n.map(|n| n - 1))),
        })
    }

    pub(super) fn crash_point() -> std::io::Result<()> {
        CRASH_AFTER.with(|c| match c.get() {
            Some(0) => Err(std::io::Error::other("crash")),
            n => Ok(c.set(n.map(|n| n - 1))),
        })
    }

    /// One of each change: an overwrite that extends, a truncation, a new
    /// file and directory, a removed file and directory, and renames, one
    /// over an existing file.
    fn change(j: &mut Journal, v: &Path) {
        write(j, v, "a", 6, b"WORLD, again");
        let f = File::options().read(true).write(true).open(v.join("f")).unwrap();
        j.writing(&f, 0, 2).unwrap();
        f.set_len(0).unwrap();
        j.creating(&v.join("n")).unwrap();
        std::fs::write(v.join("n"), "new").unwrap();
        j.created(&v.join("n")).unwrap();
        j.creating(&v.join("m")).unwrap();
        std::fs::create_dir(v.join("m")).unwrap();
        j.remove(&v.join("d/b"), 3).unwrap();
        j.removing_dir(&v.join("e")).unwrap();
        std::fs::remove_dir(v.join("e")).unwrap();
        rename(j, v, "a", "d/a2");
        rename(j, v, "n", "f");
    }

    /// An uncommitted request is rolled back when the volume is next opened,
    /// as after a failed request or a crash.
    #[test]
    fn what_isnt_committed_is_rolled_back() {
        let v = volume();
        let before = tree(&v);
        change(&mut open(&v), &v);
        assert_ne!(tree(&v), before);
        open(&v);
        assert_eq!(tree(&v), before);
    }

    /// A commit keeps everything, and leaves nothing to roll back.
    #[test]
    fn what_is_committed_stays() {
        let v = volume();
        let mut j = open(&v);
        change(&mut j, &v);
        let after = tree(&v);
        j.commit().unwrap();
        assert!(j.log.is_none() && j.held == (0, 0));
        open(&v);
        assert_eq!(tree(&v), after);
        assert_eq!(std::fs::read(v.join("d/a2")).unwrap(), b"hello WORLD, again");
        assert_eq!(std::fs::read(v.join("f")).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(&j.dir).unwrap().count(), 0, "the journal is empty");
    }

    /// A real filesystem error must preserve the undo, not turn a failed
    /// rollback into a successful open. Repairing the obstruction permits retry.
    #[test]
    fn recovery_failure_preserves_the_undo() {
        let v = volume();
        let before = tree(&v);
        let mut j = open(&v);
        write(&mut j, &v, "a", 0, b"CHANGED");
        let log = j.dir.join(LOG);
        drop(j);
        let parked = v.parent().unwrap().join("parked");
        std::fs::rename(v.join("a"), &parked).unwrap();
        // A directory is syncable but cannot receive a file's undo bytes.
        std::fs::create_dir(v.join("a")).unwrap();
        let saved = std::fs::read(&log).unwrap();
        for _ in 0..2 {
            assert!(reopen(&v, 1 << 30).is_err());
            assert_eq!(std::fs::read(&log).unwrap(), saved);
        }
        std::fs::remove_dir(v.join("a")).unwrap();
        std::fs::rename(parked, v.join("a")).unwrap();
        open(&v);
        assert_eq!(tree(&v), before);
    }

    #[test]
    fn failed_commit_cannot_be_retried_as_success() {
        let v = volume();
        let before = tree(&v);
        let mut j = open(&v);
        write(&mut j, &v, "a", 0, b"CHANGED");
        let parked = v.parent().unwrap().join("parked");
        std::fs::rename(v.join("a"), &parked).unwrap();
        std::os::unix::fs::symlink("a", v.join("a")).unwrap();
        assert!(j.commit().is_err());
        assert!(j.commit().is_err(), "an earlier sync failure must not look committed");
        assert!(j.creating(&v.join("later")).is_err());
        std::fs::remove_file(v.join("a")).unwrap();
        std::fs::rename(parked, v.join("a")).unwrap();
        drop(j);
        open(&v);
        assert_eq!(tree(&v), before);
    }

    /// Fail each host sync in turn. This exercises EIO handling on real
    /// files, not loss of unsynced writes from the kernel's page cache.
    #[test]
    fn sync_failures_preserve_a_recoverable_state() {
        for phase in ["record", "commit", "recover"] {
            let mut cuts = 0;
            for after in 0..64 {
                let v = volume();
                let before = tree(&v);
                let mut j = open(&v);
                if phase != "record" {
                    change(&mut j, &v);
                }
                let changed = tree(&v);
                let log = j.dir.join(LOG);
                FAIL_SYNC_AFTER.set(Some(after));
                let failed = match phase {
                    "record" => {
                        let f = File::options().write(true).open(v.join("a")).unwrap();
                        j.writing(&f, 0, 7).is_err()
                    }
                    "commit" => j.commit().is_err(),
                    _ => reopen(&v, 1 << 30).is_err(),
                };
                FAIL_SYNC_AFTER.set(None);
                if failed && phase != "recover" {
                    assert!(j.commit().is_err(), "{phase}: a failed sync poisons the instance");
                }
                // Once the log is unlinked, a failed commit sync has an
                // ambiguous durable outcome; on this running kernel it is new.
                let expected = if phase == "commit" && !log.exists() { &changed } else { &before };
                drop(j);
                open(&v);
                assert_eq!(&tree(&v), expected, "{phase} sync cut {after}");
                std::fs::remove_dir_all(v.parent().unwrap()).unwrap();
                if !failed { break; }
                cuts += 1;
            }
            assert!((4..64).contains(&cuts), "{phase}: must cover all sync boundaries, {cuts}");
            println!("journal {phase}: {cuts} injected sync failures recovered correctly");
        }
    }

    /// Recovery that crashes after any number of undos resumes from there,
    /// and a record cut short by a crash (never acted on) is ignored.
    #[test]
    fn recovery_survives_crashing_part_way() {
        for after in 0..12 {
            let v = volume();
            let before = tree(&v);
            let mut j = open(&v);
            change(&mut j, &v);
            let mut log = File::options().append(true).open(j.dir.join(LOG)).unwrap();
            log.write_all(&encode(&Undo::Created(PathBuf::from("o/a")))[..9]).unwrap();
            drop(j);
            CRASH_AFTER.with(|c| c.set(Some(after)));
            let crashed = reopen(&v, 1 << 30).is_err();
            CRASH_AFTER.with(|c| c.set(None));
            open(&v);
            assert_eq!(tree(&v), before, "crashed after {after} undos: {crashed}");
        }
    }

    /// A file the guest still holds open after removing it lives in the
    /// journal; writing to it there can't spoil what a rollback restores.
    #[test]
    fn writes_to_a_removed_file_are_journaled_too() {
        let v = volume();
        let before = tree(&v);
        let mut j = open(&v);
        let file = File::options().read(true).write(true).open(v.join("a")).unwrap();
        j.remove(&v.join("a"), 11).unwrap();
        let p = j.local_file(&file).expect("the journal is local");
        assert!(p.starts_with("~journal"), "{p:?}");
        j.writing(&file, 0, 5).unwrap();
        file.write_all_at(b"HELLO", 0).unwrap();
        drop(j);
        open(&v);
        assert_eq!(tree(&v), before);
    }

    /// A guest path is the volume's only if it is inside it after `..`: its
    /// journal and everything else are out of reach.
    #[test]
    fn only_the_volume_is_local() {
        let v = volume();
        let j = open(&v);
        assert_eq!(j.local(&v.join("d/../a")), Some(v.join("a")));
        assert_eq!(j.local(&v.join("../~journal/o/log")), None);
        assert_eq!(j.local(&v.join("../other/x")), None);
        assert_eq!(j.local(&v.join("../../../etc/passwd")), None);
        assert_eq!(j.local(Path::new("/tmp/x")), None);
    }

    /// Creating what already exists, or removing a directory that isn't
    /// there, records nothing: a failed `mkdir` on a directory, a file named
    /// with a trailing `/`, or a failed `rmdir` can't make a rollback delete
    /// what was there, create what never was, or make recovery fail.
    #[test]
    fn failed_creates_and_removes_are_not_undone() {
        let v = volume();
        let before = tree(&v);
        let mut j = open(&v);
        for p in ["d", "a/", "", "e"] {
            j.creating(&v.join(p)).unwrap();
        }
        for p in ["missing", "a", "d/missing"] {
            j.removing_dir(&v.join(p)).unwrap();
        }
        assert!(j.log.is_none());
        drop(j);
        open(&v);
        assert_eq!(tree(&v), before);
    }

    /// A file's size is recorded once per file, not per path: writing one
    /// file, then renaming another onto its path and extending that, rolls
    /// both back.
    #[test]
    fn sizes_follow_files_not_paths() {
        let v = volume();
        let before = tree(&v);
        let mut j = open(&v);
        write(&mut j, &v, "a", 0, b"A");
        rename(&mut j, &v, "a", "b");
        rename(&mut j, &v, "f", "a");
        write(&mut j, &v, "a", 2, b"23456789");
        drop(j);
        open(&v);
        assert_eq!(tree(&v), before);
    }

    /// Failed overwrites keep both the dirty source and a restored new
    /// target's inode/descriptor tracking intact.
    #[test]
    fn failed_rename_keeps_dirty_source_and_restored_creation_tracked() {
        let v = volume();
        let before = tree(&v);
        let mut j = open(&v);
        write(&mut j, &v, "a", 0, b"dirty");
        let to = v.join("new");
        j.creating(&to).unwrap();
        std::fs::write(&to, b"new").unwrap();
        j.created(&to).unwrap();
        let handle = Arc::new(File::open(&to).unwrap());
        j.opened(&handle).unwrap();
        let ino = handle.metadata().unwrap().ino();
        let slot = j.remove(&to, 3).unwrap();
        let at = j.renaming(&v.join("a"), &to).unwrap();
        assert!(
            j.touched.contains(Path::new("o/a")),
            "dirty source must still be synced after failure"
        );
        j.restore(&slot, &to, 3).unwrap();
        j.cancel_rename(at).unwrap();
        assert_eq!(j.held.1, 0);
        assert_eq!(std::fs::metadata(&to).unwrap().ino(), ino);
        assert!(j.created.contains_key(&ino));
        j.unlink(&to, 3).unwrap();
        assert_eq!(j.held.1, 3, "the open, discarded creation still occupies storage");
        drop(handle);
        j.charge(0, 0).unwrap();
        assert_eq!(j.held.1, 0);
        drop(j);
        open(&v);
        assert_eq!(tree(&v), before);
        std::fs::remove_dir_all(v.parent().unwrap()).unwrap();
    }

    /// Cancellation must survive interrupted recovery, including later
    /// discard and reuse of the failed rename's source and target paths.
    #[test]
    fn cancelled_renames_survive_interrupted_recovery() {
        for after in 0..10 {
            let v = volume();
            let before = tree(&v);
            let mut j = open(&v);
            let (from, to) = (v.join("new"), v.join("f"));
            j.creating(&from).unwrap();
            std::fs::write(&from, b"new").unwrap();
            j.created(&from).unwrap();
            let slot = j.remove(&to, 2).unwrap();
            let at = j.renaming(&from, &to).unwrap();
            j.restore(&slot, &to, 2).unwrap();
            j.cancel_rename(at).unwrap();
            j.unlink(&from, 3).unwrap();
            j.unlink(&to, 2).unwrap();
            j.creating(&to).unwrap();
            std::fs::write(&to, b"replacement").unwrap();
            j.created(&to).unwrap();
            drop(j);
            CRASH_AFTER.with(|c| c.set(Some(after)));
            let _ = reopen(&v, 4096);
            CRASH_AFTER.with(|c| c.set(None));
            open(&v);
            assert_eq!(tree(&v), before, "recovery interrupted at {after}");
            std::fs::remove_dir_all(v.parent().unwrap()).unwrap();
        }
    }

    /// A file opened write-only can be written over: the bytes it replaces
    /// are read through a handle of the journal's own.
    #[test]
    fn write_only_files_can_be_overwritten() {
        let v = volume();
        let before = tree(&v);
        let mut j = open(&v);
        let file = File::options().write(true).open(v.join("a")).unwrap();
        j.writing(&file, 0, 5).unwrap();
        file.write_all_at(b"HELLO", 0).unwrap();
        drop(j);
        open(&v);
        assert_eq!(tree(&v), before);
    }

    /// A change the full journal refused stays refused when retried: the
    /// failed attempt can't leave the file's size counted as recorded, which
    /// would let an append through with nothing to undo it.
    #[test]
    fn refused_changes_stay_refused() {
        let v = volume();
        let before = tree(&v);
        let mut j = reopen(&v, 30).unwrap();
        let a = File::options().read(true).write(true).open(v.join("a")).unwrap();
        for _ in 0..2 {
            if j.writing(&a, 11, 16).is_ok() {
                a.write_all_at(b"MORE!", 11).unwrap();
            }
        }
        drop(j);
        open(&v);
        assert_eq!(tree(&v), before);
    }

    /// What the journal holds is bounded: past it, a change is refused as a
    /// full disk, before it is made.
    #[test]
    fn the_journal_is_bounded() {
        let v = volume();
        let mut j = reopen(&v, 100).unwrap();
        let a = File::options().read(true).write(true).open(v.join("a")).unwrap();
        j.writing(&a, 0, 5).unwrap();
        for _ in 0..2 {
            assert_eq!(j.writing(&a, 0, 11).unwrap_err().kind(), ErrorKind::StorageFull);
            assert_eq!(j.remove(&v.join("f"), 2).unwrap_err().kind(), ErrorKind::StorageFull);
            assert_eq!(j.held.1, 0, "a refused removal retains no bytes");
        }
        assert_eq!(
            j.remove(&v.join("f"), 1 << 20).unwrap_err().kind(),
            ErrorKind::StorageFull
        );
        assert!(v.join("f").exists());
    }

    /// Saved ranges stay sorted, disjoint and merged, whatever order they
    /// are saved in, so the list stays as short as the file's gaps allow.
    #[test]
    fn saved_ranges_merge() {
        let mut ranges = vec![];
        for r in [4..6, 0..2, 8..9, 2..4, 6..8] {
            save(&mut ranges, r);
        }
        assert_eq!(ranges, [0..9]);
        save(&mut ranges, 10..12);
        assert_eq!(ranges, [0..9, 10..12]);
    }

    /// Overlap, rename, and truncate/regrow must restore request-start
    /// bytes, not intermediate versions. Repeat recovery at every record.
    #[test]
    fn original_bytes_are_saved_once() {
        for after in 0..9 {
            let v = volume();
            let before = tree(&v);
            let mut j = reopen(&v, 512).unwrap();
            write(&mut j, &v, "a", 3, b"XYZ");
            write(&mut j, &v, "a", 1, b"12345678");
            rename(&mut j, &v, "a", "renamed");
            let f = File::options().write(true).open(v.join("renamed")).unwrap();
            j.writing(&f, 2, 11).unwrap();
            f.set_len(2).unwrap();
            for _ in 0..1000 {
                write(&mut j, &v, "renamed", 0, b"abcdefghijklmnop");
            }
            let payload: usize = decode(&std::fs::read(j.dir.join(LOG)).unwrap())
                .iter()
                .map(|(_, u)| match u {
                    Undo::Bytes(_, _, bytes) => bytes.len(),
                    _ => 0,
                })
                .sum();
            assert_eq!(payload, 11, "each original byte once; extensions need no backup");
            drop(j);
            CRASH_AFTER.with(|c| c.set(Some(after)));
            let _ = reopen(&v, 512);
            CRASH_AFTER.with(|c| c.set(None));
            open(&v);
            assert_eq!(tree(&v), before, "recovery interrupted at {after}");
        }
    }

    /// New identities can disappear, be replaced, and reuse the same
    /// names without retaining their contents or confusing original files.
    #[test]
    fn discarded_creations_do_not_change_recovery() {
        for after in 0..14 {
            let v = volume();
            let before = tree(&v);
            let mut j = reopen(&v, 4096).unwrap();
            let create = |j: &mut Journal, name: &str| {
                let p = v.join(name);
                j.creating(&p).unwrap();
                std::fs::write(&p, [9; 2048]).unwrap();
                j.created(&p).unwrap();
            };
            create(&mut j, "new");
            rename(&mut j, &v, "new", "moved");
            create(&mut j, "new");
            rename(&mut j, &v, "a", "new"); // Discard the new target, not original a.
            write(&mut j, &v, "new", 0, b"CHANGED");
            j.unlink(&v.join("new"), 11).unwrap(); // Original a must survive.
            j.unlink(&v.join("moved"), 2048).unwrap();
            create(&mut j, "new");
            write(&mut j, &v, "new", 0, b"different");
            j.unlink(&v.join("new"), 2048).unwrap();
            assert_eq!(j.held.1, 11);
            assert!(j.created.is_empty(), "no stale identity can survive inode reuse");
            let count = decode(&std::fs::read(j.dir.join(LOG)).unwrap()).len();
            assert!(count < 14, "exercise every recovery cut");
            drop(j);
            CRASH_AFTER.with(|c| c.set(Some(after)));
            let _ = reopen(&v, 4096);
            CRASH_AFTER.with(|c| c.set(None));
            open(&v);
            assert_eq!(tree(&v), before, "recovery interrupted at {after}");
        }
    }
}
