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
//! Writes then happen in place, so open files stay valid. Records made
//! back to back, with no change between them, share one sync. Committing
//! syncs what changed, then replaces the key the log's records are signed
//! with, which voids them all. Rolling back replays the log backwards, which
//! is what [`recover`] does when an instance starts, so a failed request (its
//! instance is restarted) and a crash take the same path. Recovery drops each
//! record once undone, so a crash during it resumes where it stopped.
//!
//! The log lives beside the volume, at `<objects>/~journal/<object>/log`,
//! which no object id can name and no guest path reaches. It is made once
//! and kept: its first block holds a format tag and the key, so a commit
//! writes and syncs a block that already exists. Records follow, each one
//! signed, so the end of a request's records is the first that isn't: what
//! lies past them is left from earlier requests, and a guest chose some of
//! those bytes (old file contents), but it can't sign them without the key,
//! which is random and never leaves the log. Paths in it are relative to
//! `<objects>`: in the volume, or in the journal for a file the guest still
//! writes after removing it. The log, and the removed files, may each hold
//! as much as the object's quota during a request; between requests the log
//! keeps at most `KEEP` bytes.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::ErrorKind;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Weak};

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, RenameFlags, mkdirat, openat, renameat_with, statat, unlinkat};
use rustix::io::Errno;
use sha2::{Digest, Sha256};

use crate::{committed, config::CaptureConfig, metrics::thread_cpu_ns, object::ObjShared};

const LOG: &str = "log";
/// The log's format tag, then the key, in its first block; records start in
/// the next, so writing them never rewrites the key.
const MAGIC: &[u8; 8] = b"nanovmj1";
const HEADER: u64 = 4096;
/// After a commit, a longer log is cut back to this.
const KEEP: u64 = HEADER + (64 << 10);
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
    /// The volume's device and inode: it never moves, so a descriptor for it
    /// needs no asking where it is.
    root: (u64, u64),
    dir: PathBuf,
    /// Open while the current request has changes.
    log: Option<File>,
    /// What the current request's records are signed with.
    key: [u8; 8],
    /// Where the next record goes.
    end: u64,
    /// Records are written that aren't synced yet.
    unsynced: bool,
    /// Files (by inode) whose size is recorded, and what to sync on commit.
    sized: HashMap<u64, Saved>,
    /// Successful creations, with every guest open of that inode. Weak
    /// handles include stream ownership without keeping the file alive.
    created: HashMap<u64, Vec<Weak<File>>>,
    /// Unlinked new files still held open: their removal-time bytes remain
    /// charged to the retained-file budget until the last handle closes.
    unlinked: Vec<(Vec<Weak<File>>, u64)>,
    touched: HashSet<PathBuf>,
    /// Where renames put what they moved, for a captured version to take whole.
    moved: HashSet<PathBuf>,
    slots: u64,
    /// Bytes the log holds, and the removed files; each may reach `max`.
    held: (u64, u64),
    max: u64,
    capture: Option<CaptureConfig>,
    poisoned: bool,
    /// Waits for the disk aren't charged to its CPU budget.
    obj: Arc<ObjShared>,
}

impl Journal {
    /// The journal of `volume` (an absolute path without symlinks), whose
    /// log and removed files may each hold `max` bytes, after rolling back
    /// what a previous instance left uncommitted.
    pub fn open(volume: &Path, max: u64, obj: Arc<ObjShared>, capture: Option<&CaptureConfig>) -> std::io::Result<Journal> {
        let base = volume.parent().expect("a volume has a parent").to_path_buf();
        let dir = base.join("~journal").join(volume.file_name().expect("a volume has a name"));
        let key = match recover(&base, &dir, &obj)? {
            Some(key) => key,
            None => key()?,
        };
        if capture.is_none() && committed::directory(&base, &dir).try_exists()? {
            return Err(std::io::Error::other("committed history requires capture to remain enabled"));
        }
        let md = std::fs::metadata(volume)?;
        Ok(Journal {
            base,
            volume: volume.to_path_buf(),
            root: (md.dev(), md.ino()),
            dir,
            log: None,
            key,
            end: HEADER,
            unsynced: false,
            sized: HashMap::new(),
            created: HashMap::new(),
            unlinked: vec![],
            touched: HashSet::new(),
            moved: HashSet::new(),
            slots: 0,
            held: (0, 0),
            max,
            capture: capture.cloned(),
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
        let md = dir.metadata().ok()?;
        (md.nlink() > 0).then_some(())?;
        let at = if (md.dev(), md.ino()) == self.root { self.volume.clone() } else { fd_path(dir)? };
        self.local(&at.join(path))
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
        self.append(undo)?;
        self.sync_log()
    }

    /// Writes `undo` to the log, unsynced: before the change it undoes is
    /// made, [`Self::sync_log`] must run. Returns where it is.
    fn append(&mut self, undo: Undo) -> std::io::Result<u64> {
        self.check()?;
        let rec = encode(&self.key, &undo);
        self.charge(rec.len() as u64, 0)?;
        let at = self.end;
        // An I/O failure may leave a complete or partial record. Do not
        // permit more mutations or a commit until recovery has run.
        self.or_poison(|j| {
            if j.log.is_none() {
                j.log = Some(j.open_log()?);
            }
            j.log.as_ref().expect("opened above").write_all_at(&rec, at)
        })?;
        self.end += rec.len() as u64;
        self.unsynced = true;
        Ok(at)
    }

    /// Makes the records written so far durable.
    fn sync_log(&mut self) -> std::io::Result<()> {
        if self.unsynced {
            self.or_poison(|j| {
                #[cfg(test)]
                tests::sync_point()?;
                j.log.as_ref().expect("records were written").sync_data()
            })?;
            self.unsynced = false;
        }
        Ok(())
    }

    /// Opens the log; if the object never had one, makes it under another
    /// name, so the log is whole whenever it exists, along with the
    /// directories it is in.
    fn open_log(&self) -> std::io::Result<File> {
        let path = self.dir.join(LOG);
        match File::options().write(true).open(&path) {
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            log => return log,
        }
        std::fs::create_dir_all(&self.dir)?;
        let new = self.dir.join("new");
        let log = File::options().write(true).create(true).truncate(true).open(&new)?;
        log.write_all_at(&[&MAGIC[..], &self.key].concat(), 0)?;
        #[cfg(test)]
        tests::sync_point()?;
        log.sync_data()?;
        std::fs::rename(new, path)?;
        for d in [&self.dir, self.dir.parent().expect("in ~journal"), &self.base] {
            sync(d)?;
        }
        Ok(log)
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
        let saved = self.sized.get(&md.ino());
        // The parts of `[at, end)` within the original size not saved yet.
        let end = end.min(saved.map_or(md.len(), |s| s.len));
        let (mut gaps, mut from) = (vec![], at);
        for r in saved.iter().flat_map(|s| &s.ranges).filter(|r| r.end > at && r.start < end) {
            if r.start > from {
                gaps.push(from..r.start);
            }
            from = r.end;
        }
        if from < end {
            gaps.push(from..end);
        }
        if saved.is_some() && gaps.is_empty() {
            return Ok(());
        }
        if saved.is_none() {
            self.append(Undo::Size(p.clone(), md.len()))?;
        }
        // Read through a handle of our own: the guest's may be write-only.
        let current = if gaps.is_empty() { None } else { Some(File::open(self.base.join(&p))?) };
        let mut kept = vec![];
        for gap in gaps {
            for at in gap.clone().step_by(CHUNK as usize) {
                let mut old = vec![0; (gap.end - at).min(CHUNK) as usize];
                current.as_ref().expect("opened for gaps").read_exact_at(&mut old, at)?;
                kept.push(at..at + old.len() as u64);
                self.append(Undo::Bytes(p.clone(), at, old))?;
            }
        }
        self.sync_log()?;
        // Only a durable record protects a size or a range, including when
        // a later chunk fails and the caller retries the whole write.
        let saved = self.sized.entry(md.ino()).or_insert_with(|| Saved { len: md.len(), ranges: vec![] });
        for r in kept {
            save(&mut saved.ranges, r);
        }
        self.touched.insert(p);
        Ok(())
    }

    /// Before the entry at `path` is created, if nothing is there yet.
    pub fn creating(&mut self, path: &Path) -> std::io::Result<()> {
        let path: PathBuf = path.components().collect();
        match std::fs::symlink_metadata(&path) {
            Ok(_) => return Ok(()),
            // Under a file, or a name or path too long: the create would fail
            // the same way, unless (a path too long for us, not the guest's
            // descriptor) it made what we couldn't reach to undo. Refuse.
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
            Err(_) => {}
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
        match std::fs::symlink_metadata(path) {
            Ok(md) if md.is_dir() => {}
            // As in `creating`: only a missing entry or a file is left to the syscall.
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
            _ => return Ok(()),
        }
        let p = self.rel(path);
        self.record(Undo::RemovedDir(p.clone()))?;
        self.touch_parent(&p);
        Ok(())
    }

    /// Removes the file at `path`, of `bytes`, by moving it into the
    /// journal: where it now is.
    fn remove(&mut self, path: &Path, bytes: u64) -> std::io::Result<PathBuf> {
        self.charge(0, bytes)?;
        let slot = self.slot();
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

    fn slot(&mut self) -> PathBuf {
        self.slots += 1;
        self.dir.join(self.slots.to_string())
    }

    /// Before `from` is renamed to `to`, where `replaced` is: a file there
    /// is moved into the journal, to the slot returned; an empty directory
    /// is recorded to be made again. One sync covers the records. Also
    /// returns where the rename's own record is, to cancel it if the rename
    /// fails.
    pub fn renaming(
        &mut self,
        from: &Path,
        to: &Path,
        replaced: Option<&std::fs::Metadata>,
    ) -> std::io::Result<(u64, Option<PathBuf>)> {
        within_path_max(to, from)?;
        let bytes = replaced.filter(|md| md.is_file()).map_or(0, |md| md.len());
        self.charge(0, bytes)?;
        let (from, to) = (self.rel(from), self.rel(to));
        let staged = (|| -> std::io::Result<_> {
            let slot = match replaced {
                Some(md) if md.is_file() => {
                    let slot = self.slot();
                    self.append(Undo::Renamed(to.clone(), self.rel(&slot)))?;
                    Some(slot)
                }
                Some(_) => {
                    self.append(Undo::RemovedDir(to.clone()))?;
                    None
                }
                None => None,
            };
            let at = self.append(Undo::Renamed(from, to.clone()))?;
            self.sync_log()?;
            Ok((at, slot))
        })();
        let (at, slot) = staged.inspect_err(|_| self.held.1 -= bytes)?;
        if let Some(slot) = &slot {
            if let Err(e) = std::fs::rename(self.base.join(&to), slot) {
                self.held.1 -= bytes;
                self.cancel_rename(at)?;
                return Err(e);
            }
            self.touch_parent(&to);
        }
        Ok((at, slot))
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
        self.moved = self.moved.drain().map(moved).chain([to.clone()]).collect();
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
            let log = j.log.as_ref().expect("the rename was recorded");
            // Past the signature, the length, and the tag's own length.
            log.write_all_at(&[5], at + 24)?;
            #[cfg(test)]
            tests::sync_point()?;
            log.sync_data()
        })
    }

    /// Runs `f`, whose CPU isn't charged to the guest; if it fails, the
    /// journal needs recovery.
    fn or_poison(&mut self, f: impl FnOnce(&mut Self) -> std::io::Result<()>) -> std::io::Result<()> {
        let t0 = thread_cpu_ns();
        let result = f(self);
        self.obj.uncharge(t0);
        self.poisoned |= result.is_err();
        result
    }

    /// Whether the current request has recorded a change.
    pub fn changed(&self) -> bool {
        self.log.is_some()
    }

    pub fn check(&self) -> std::io::Result<()> {
        if self.poisoned {
            return Err(std::io::Error::other("journal requires recovery"));
        }
        Ok(())
    }

    /// What this request changed in the volume, given the `paths` it
    /// touched: the directories whose entries changed, then each file
    /// changed in place as the ranges that changed, and anything new or
    /// moved whole.
    fn changes(&self, paths: &[PathBuf]) -> std::io::Result<committed::Changes> {
        let inside = |p: &Path| p.strip_prefix(&self.volume).ok().map(Path::to_path_buf);
        let moved: Vec<_> = self.moved.iter().filter_map(|m| inside(&self.base.join(m))).collect();
        let mut c = committed::Changes::default();
        for p in paths {
            let Some(rel) = inside(p).filter(|rel| !moved.iter().any(|m| rel.starts_with(m))) else { continue };
            let md = match std::fs::symlink_metadata(p) {
                Ok(md) => md,
                // Gone: the listing of a directory it was in says so.
                Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => continue,
                Err(e) => return Err(e),
            };
            match self.sized.get(&md.ino()) {
                _ if md.is_dir() => c.listed.push(rel),
                Some(saved) => {
                    let grown = saved.len..md.len();
                    let ranges = saved.ranges.iter().chain(Some(&grown).filter(|g| !g.is_empty()));
                    let ranges = ranges.filter(|r| r.start < md.len()).map(|r| r.start..r.end.min(md.len())).collect();
                    c.patched.push((rel, ranges));
                }
                None => c.whole.push(rel),
            }
        }
        c.whole.extend(moved.into_iter().filter(|m| self.volume.join(m).exists()));
        Ok(c)
    }

    fn touch_parent(&mut self, p: &Path) {
        self.touched.insert(p.parent().unwrap_or(Path::new("")).to_path_buf());
    }

    /// Makes the request's changes durable and forgets how to undo them.
    /// It waits for the disk, so callers run it off the object's task: its
    /// time isn't charged as CPU.
    pub fn commit(&mut self) -> std::io::Result<()> {
        self.check()?;
        // With nothing to commit, the lease still decides whether its
        // publishes go out.
        let Some(log) = self.log.take() else { return self.obj.lease_epoch().map(drop) };
        // Any error leaves the outcome uncertain. In particular, taking the
        // log must not make a second commit look like an empty successful one.
        self.poisoned = true;
        let paths: Vec<_> = self.touched.drain().map(|p| self.base.join(p)).collect();
        sync_all(&paths)?;
        let (out, old) = (committed::directory(&self.base, &self.dir), self.key);
        if let Some(cap) = &self.capture {
            let epoch = self.obj.lease_epoch()?;
            committed::prepare(&self.volume, &out, &self.obj, epoch, cap, &old, Some(&self.changes(&paths)?))?;
        }
        // Only while the node holds the object's lease, at the decision: a
        // lapsed lease leaves the records signed, so recovery undoes the
        // change and discards what capture staged.
        self.obj.lease_epoch()?;
        // A new key is the commit: no record is signed with it.
        self.key = key()?;
        log.write_all_at(&self.key, MAGIC.len() as u64)?;
        #[cfg(test)]
        tests::sync_point()?;
        log.sync_data()?;
        if let Some(cap) = &self.capture {
            let sequence = committed::publish(&out, &old)?;
            if cap.remote_ack {
                crate::ship::stored(&out, sequence)?;
            }
        }
        self.moved.clear();
        if self.end > KEEP {
            log.set_len(KEEP)?;
        }
        (self.end, self.unsynced) = (HEADER, false);
        clear(&self.dir)?;
        self.sized.clear();
        self.created.clear();
        self.held = (0, self.unlinked.iter().map(|(_, bytes)| bytes).sum());
        self.poisoned = false;
        Ok(())
    }
}

/// Rolls back the log in `dir` if there is one, then empties `dir` but for
/// the log, and returns its key. Failure leaves the current undo intact and
/// refuses to start the object. Only already-applied or never-performed
/// operations may be skipped.
fn recover(base: &Path, dir: &Path, obj: &ObjShared) -> std::io::Result<Option<[u8; 8]>> {
    let log = match std::fs::read(dir.join(LOG)) {
        Ok(log) => log,
        Err(e) if e.kind() == ErrorKind::NotFound => return clear(dir).map(|()| None),
        Err(e) => return Err(e),
    };
    let Some(key) = log.strip_prefix(MAGIC).and_then(|k| k.get(..8)?.try_into().ok()) else {
        return Err(std::io::Error::new(ErrorKind::InvalidData, "not a journal log"));
    };
    let file = File::options().write(true).open(dir.join(LOG))?;
    let undos = decode(&log, &key);
    // What commits staged settles first; a key seen here may not be durable yet.
    committed::settle(&committed::directory(base, dir), obj, &key, !undos.is_empty(), || {
        #[cfg(test)]
        tests::sync_point()?;
        file.sync_data()
    })?;
    // Each record is dropped from the log once undone and synced, so a crash
    // here resumes where it stopped; the undo it was in may run twice, which
    // each tolerates. A record cut short by a crash was never acted on.
    for (at, undo) in undos.into_iter().rev() {
        let open = |p: &Path| -> std::io::Result<File> {
            let (dir, name) = parent_at(base, p)?;
            Ok(openat(&dir, name, OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?.into())
        };
        match &undo {
            Undo::Size(p, n) => open(p).and_then(|f| f.set_len(*n)),
            Undo::Bytes(p, at, data) => open(p).and_then(|f| f.write_all_at(data, *at)),
            Undo::Created(p) => parent_at(base, p).and_then(|(dir, name)| match is_dir_at(&dir, name)? {
                Some(true) => Ok(unlinkat(&dir, name, AtFlags::REMOVEDIR)?),
                Some(false) => Ok(unlinkat(&dir, name, AtFlags::empty())?),
                None => Ok(()),
            }),
            Undo::RemovedDir(p) => parent_at(base, p).and_then(|(dir, name)| match mkdirat(&dir, name, 0o777.into()) {
                Err(e) if e == Errno::EXIST && is_dir_at(&dir, name)? == Some(true) => Ok(()),
                made => Ok(made?),
            }),
            Undo::Renamed(from, to) => (|| {
                // The source is present after an earlier replay or a failed
                // syscall. Both may be absent for a discarded new file.
                let (a, x) = parent_at(base, from)?;
                if is_dir_at(&a, x)?.is_none() {
                    let (b, y) = parent_at(base, to)?;
                    if is_dir_at(&b, y)?.is_some() {
                        renameat_with(&b, y, &a, x, RenameFlags::empty())?;
                    }
                }
                Ok(())
            })(),
            Undo::Cancelled => Ok(()),
        }
        // A path that can't name an entry has nothing to undo (N-502, N-503).
        .or_else(|e| if shape(&e) { Ok(()) } else { Err(e) })?;
        let p = match &undo {
            Undo::Size(p, _) | Undo::Bytes(p, ..) => p.as_path(),
            Undo::Created(p) | Undo::RemovedDir(p) | Undo::Renamed(p, _) => p.parent().unwrap_or(Path::new("")),
            Undo::Cancelled => Path::new(""),
        };
        sync_at(base, p)?;
        if let Undo::Renamed(_, to) = &undo {
            // A cross-directory rename changes both parents. Persist both
            // before dropping its undo, including when replay runs twice.
            sync_at(base, to.parent().unwrap_or(Path::new("")))?;
        }
        #[cfg(test)]
        tests::crash_point()?;
        file.set_len(at)?;
        #[cfg(test)]
        tests::sync_point()?;
        file.sync_data()?;
    }
    file.set_len(file.metadata()?.len().min(KEEP))?;
    clear(dir)?;
    Ok(Some(key))
}

/// The directory holding `p` (relative to `base`), opened one name at a
/// time and never through a symlink, and `p`'s name in it. Recovery reaches
/// entries so: the log's paths are relative to `base`, which can move deeper
/// between a crash and the recovery after it (a backup restored into a
/// longer root), so a real entry's whole path can be past PATH_MAX while
/// each of its names fits. Only a name past NAME_MAX is then too long, and
/// nothing can have one (N-690).
fn parent_at<'a>(base: &Path, p: &'a Path) -> std::io::Result<(File, &'a OsStr)> {
    let name = p.file_name().ok_or(ErrorKind::InvalidInput)?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    // `openat` and `renameat2`: on x86_64 rustix's `open` is SYS_open, and
    // on aarch64 its `renameat` is SYS_renameat, neither in the jail's list.
    let mut dir = openat(CWD, base, flags, Mode::empty())?;
    for c in p.parent().expect("named").components() {
        let Component::Normal(c) = c else { return Err(ErrorKind::InvalidInput.into()) };
        dir = openat(&dir, c, flags | OFlags::NOFOLLOW, Mode::empty())?;
    }
    Ok((dir.into(), name))
}

/// Whether `name` in `dir` is a directory; `None` if nothing can be there.
fn is_dir_at(dir: &File, name: &OsStr) -> std::io::Result<Option<bool>> {
    match statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(std::io::Error::from) {
        Ok(st) => Ok(Some(FileType::from_raw_mode(st.st_mode).is_dir())),
        Err(e) if shape(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Syncs `p` (relative to `base`, reached as by [`parent_at`]; `""` is the
/// base) unless nothing can be there.
fn sync_at(base: &Path, p: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    tests::sync_point()?;
    let file = match p.file_name() {
        None => File::open(base),
        Some(_) => parent_at(base, p).and_then(|(dir, name)| {
            Ok(openat(&dir, name, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?.into())
        }),
    };
    match file {
        Err(e) if shape(&e) => Ok(()),
        file => file?.sync_all(),
    }
}

/// Whether recovery has nothing to undo at a path: [`no_entry`], or a
/// symlink (only made from outside), which recovery never follows.
fn shape(e: &std::io::Error) -> bool {
    no_entry(e) || e.raw_os_error() == Some(libc::ELOOP)
}

/// A key no guest can guess.
fn key() -> std::io::Result<[u8; 8]> {
    let mut key = [0; 8];
    // SAFETY: getentropy writes at most `key.len()` bytes into `key`.
    if unsafe { libc::getentropy(key.as_mut_ptr().cast(), key.len()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(key)
}

/// A record's signature: SHA-256 of the key and its body, with a cancelled
/// rename's tag read as the rename's, since cancelling changes only that.
fn sign(key: &[u8; 8], body: &[u8]) -> [u8; 8] {
    let tag = [if body[8] == 5 { 4 } else { body[8] }];
    let sum = Sha256::new().chain_update(key).chain_update(&body[..8]).chain_update(tag).chain_update(&body[9..]);
    sum.finalize()[..8].try_into().expect("8 of 32 bytes")
}

/// A record: its signature and the length of its body, then the body: a
/// tag, then its fields; a path or bytes as a length and the bytes, a
/// number as 8 bytes.
fn encode(key: &[u8; 8], undo: &Undo) -> Vec<u8> {
    let mut buf = vec![0; 16];
    let mut put = |b: &[u8]| {
        buf.extend((b.len() as u64).to_le_bytes());
        buf.extend(b);
    };
    fn p(p: &Path) -> &[u8] {
        p.as_os_str().as_encoded_bytes()
    }
    let fields: &[&[u8]] = match undo {
        Undo::Size(a, n) => &[&[0], p(a), &n.to_le_bytes()],
        Undo::Bytes(a, at, data) => &[&[1], p(a), &at.to_le_bytes(), data],
        Undo::Created(a) => &[&[2], p(a)],
        Undo::RemovedDir(a) => &[&[3], p(a)],
        Undo::Renamed(a, b) => &[&[4], p(a), p(b)],
        Undo::Cancelled => unreachable!("only made by cancelling a rename's tag"),
    };
    fields.iter().for_each(|f| put(f));
    let len = (buf.len() - 16) as u64;
    let sum = sign(key, &buf[16..]);
    buf[..8].copy_from_slice(&sum);
    buf[8..16].copy_from_slice(&len.to_le_bytes());
    buf
}

/// The records signed with `key` in `log`, with where each starts, up to the
/// first that isn't.
fn decode(log: &[u8], key: &[u8; 8]) -> Vec<(u64, Undo)> {
    let mut at = HEADER as usize;
    std::iter::from_fn(|| {
        let rec = log.get(at..)?;
        let len = usize::try_from(u64::from_le_bytes(rec.get(8..16)?.try_into().ok()?)).ok()?;
        let body = rec.get(16..16usize.checked_add(len)?)?;
        (body.len() > 8 && rec[..8] == sign(key, body)).then_some(())?;
        let found = (at as u64, undo(body)?);
        at += 16 + len;
        Some(found)
    })
    .collect()
}

/// The undo encoded in `body`.
fn undo(mut log: &[u8]) -> Option<Undo> {
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
    let log = &mut log;
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

/// Removes everything in `dir` but the log (slots left by a commit or a
/// rollback).
fn clear(dir: &Path) -> std::io::Result<()> {
    match std::fs::read_dir(dir) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(mut entries) => entries.try_for_each(|entry| {
            let e = entry?;
            let p = e.path();
            match () {
                _ if e.file_name() == LOG => Ok(()),
                _ if e.file_type()?.is_dir() => std::fs::remove_dir_all(p),
                _ => std::fs::remove_file(p),
            }
        }),
    }
}

/// Syncs a file or a directory.
pub(crate) fn sync(p: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    tests::sync_point()?;
    File::open(p)?.sync_all()
}

/// Persist newly created app ancestors before a worker loses access to them.
pub(crate) fn sync_ancestors(dir: &Path) -> std::io::Result<()> {
    for parent in dir.canonicalize()?.ancestors() {
        sync(parent)?;
    }
    Ok(())
}

/// Syncs `paths` (as [`sync_if_there`]), up to 16 at once: a disk completes
/// concurrent syncs in about the time of one, and a request that wrote
/// 1,500 files waited seconds for them one by one.
fn sync_all(paths: &[PathBuf]) -> std::io::Result<()> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let work = || loop {
        let Some(p) = paths.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) else { return Ok(()) };
        sync_if_there(p)?;
    };
    std::thread::scope(|s| {
        let helpers: Vec<_> = (1..paths.len().div_ceil(8).min(16)).map(|_| s.spawn(work)).collect();
        helpers.into_iter().map(|h| h.join().expect("a sync does not panic")).fold(work(), Result::and)
    })
}

/// Syncs `p` unless a later change removed it or replaced an ancestor with a file.
fn sync_if_there(p: &Path) -> std::io::Result<()> {
    match sync(p) {
        Err(e) if no_entry(&e) => Ok(()),
        r => r,
    }
}

/// Whether `e` says, from a path's shape rather than the disk, that nothing
/// can be there: missing, under a file, a name or path too long. Anything
/// else (EIO, EACCES, ...) is a real failure.
fn no_entry(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory | ErrorKind::InvalidFilename)
}

/// Refuses to move `from` (and what is under it) to `to` where a path would
/// reach PATH_MAX: the journal, commits and recovery reach entries by path,
/// while a guest's descriptors reach deeper (`creating` refuses such creates).
fn within_path_max(to: &Path, from: &Path) -> std::io::Result<()> {
    let dir = std::fs::symlink_metadata(from).is_ok_and(|m| m.is_dir());
    let mut dirs: Vec<PathBuf> = Some(from.to_path_buf()).filter(|_| dir).into_iter().collect();
    let mut deepest = 0;
    while let Some(dir) = dirs.pop() {
        for e in std::fs::read_dir(&dir)? {
            // The entry's own type: no stat, and never through a symlink.
            let (p, is_dir) = e.and_then(|e| Ok((e.path(), e.file_type()?.is_dir())))?;
            deepest = deepest.max(p.as_os_str().len() - from.as_os_str().len());
            if is_dir {
                dirs.push(p);
            }
        }
    }
    if to.as_os_str().len() + deepest >= libc::PATH_MAX as usize {
        return Err(ErrorKind::InvalidFilename.into());
    }
    Ok(())
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
        Journal::open(v, max, Arc::new(ObjShared::new("app".into(), "o")), None)
    }

    fn open(v: &Path) -> Journal {
        reopen(v, 1 << 30).unwrap()
    }

    fn captured(v: &Path, cap: &CaptureConfig) -> std::io::Result<Journal> {
        Journal::open(v, 1 << 30, Arc::new(ObjShared::new("app".into(), "o")), Some(cap))
    }

    /// The volume that captured versions 1 to `sequence` of `v` restore.
    fn archive_tree(v: &Path, sequence: u64) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let base = v.parent().unwrap();
        let (versions, dest) = (base.join(format!("versions-{sequence}")), base.join(format!("restored-{sequence}")));
        let _ = std::fs::remove_dir_all(&dest);
        let _ = std::fs::remove_dir_all(&versions);
        std::fs::create_dir(&versions).unwrap();
        for n in 1..=sequence {
            let name = format!("{n:020}.tar");
            std::fs::copy(base.join("~committed/o").join(&name), versions.join(name)).unwrap();
        }
        assert_eq!(committed::restore(&versions, &dest).unwrap(), sequence);
        tree(&dest)
    }

    /// How many versions are published in `out`.
    fn published(out: &Path) -> usize {
        let names = std::fs::read_dir(out).into_iter().flatten().map(|e| e.unwrap().file_name());
        names.filter(|n| n.to_str().unwrap().ends_with(".tar")).count()
    }

    /// Stages `j`'s version as its commit would, without deciding.
    fn stage(j: &Journal, cap: &CaptureConfig) {
        let paths: Vec<_> = j.touched.iter().map(|p| j.base.join(p)).collect();
        let out = committed::directory(&j.base, &j.dir);
        committed::prepare(&j.volume, &out, &j.obj, 0, cap, &j.key, Some(&j.changes(&paths).unwrap())).unwrap();
    }

    /// Every version, restored from the snapshot before it and the changes
    /// since, is the volume as its commit left it, through random creates,
    /// overwrites, extensions, truncations, renames of files and directories
    /// (over files and empty directories), removals and type changes.
    #[test]
    fn changes_restore_every_version() {
        let cap = CaptureConfig { max_mb: 64, snapshot_every: 7, ..Default::default() };
        let v = volume();
        let out = v.parent().unwrap().join("~committed/o");
        let mut j = captured(&v, &cap).unwrap();
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let paths = ["a", "b", "f", "d", "e", "d/b", "d/s", "d/s/g", "e/h", "e/s"];
        // Files and directories made, writes, truncations, removals, renames
        // of files and of directories.
        let mut done = [0; 7];
        for step in 0..120 {
            for _ in 0..1 + next(4) {
                let (p, q) = (paths[next(10) as usize], paths[next(10) as usize]);
                let (pa, qa) = (v.join(p), v.join(q));
                let fill = vec![b'a' + next(26) as u8; 1 + next(9000) as usize];
                match std::fs::symlink_metadata(&pa) {
                    Err(_) if pa.parent().unwrap().is_dir() => {
                        j.creating(&pa).unwrap();
                        if next(3) == 0 {
                            std::fs::create_dir(&pa).unwrap();
                            done[1] += 1;
                        } else {
                            std::fs::write(&pa, &fill).unwrap();
                            j.created(&pa).unwrap();
                            done[0] += 1;
                        }
                    }
                    Err(_) => {}
                    Ok(md) => match next(6) {
                        0 | 1 if md.is_file() => {
                            write(&mut j, &v, p, next(md.len() + 10), &fill);
                            done[2] += 1;
                        }
                        2 if md.is_file() => {
                            let f = File::options().write(true).open(&pa).unwrap();
                            let size = next(md.len() + 1);
                            j.writing(&f, size, md.len()).unwrap();
                            f.set_len(size).unwrap();
                            done[3] += 1;
                        }
                        3 if md.is_file() => {
                            j.unlink(&pa, md.len()).unwrap();
                            done[4] += 1;
                        }
                        3 if std::fs::read_dir(&pa).unwrap().next().is_none() => {
                            j.removing_dir(&pa).unwrap();
                            std::fs::remove_dir(&pa).unwrap();
                            done[4] += 1;
                        }
                        _ if p != q && !q.starts_with(&format!("{p}/")) && qa.parent().unwrap().is_dir() => {
                            let target = std::fs::symlink_metadata(&qa);
                            let fits = match &target {
                                Err(_) => true,
                                Ok(t) if t.is_dir() => md.is_dir() && std::fs::read_dir(&qa).unwrap().next().is_none(),
                                Ok(_) => md.is_file(),
                            };
                            if fits {
                                rename(&mut j, &v, p, q);
                                done[5 + md.is_dir() as usize] += 1;
                            }
                        }
                        _ => {}
                    },
                }
            }
            j.commit().unwrap();
            let n = published(&out) as u64;
            assert_eq!(archive_tree(&v, n), tree(&v), "step {step}, version {n}");
        }
        assert!(published(&out) > 80 && done.iter().all(|&n| n > 3), "{} versions, {done:?}", published(&out));
    }

    /// A directory renamed over one emptied in the same request replaces it
    /// in the version: what the emptied one held is gone.
    #[test]
    fn a_directory_renamed_over_another_replaces_it() {
        let cap = CaptureConfig::default();
        let v = volume();
        let mut j = captured(&v, &cap).unwrap();
        write(&mut j, &v, "d/b", 0, b"B");
        j.commit().unwrap();
        j.unlink(&v.join("d/b"), 3).unwrap();
        j.creating(&v.join("n")).unwrap();
        std::fs::create_dir(v.join("n")).unwrap();
        j.creating(&v.join("n/c")).unwrap();
        std::fs::write(v.join("n/c"), "C").unwrap();
        j.created(&v.join("n/c")).unwrap();
        rename(&mut j, &v, "n", "d");
        j.commit().unwrap();
        assert_eq!(archive_tree(&v, 2), tree(&v));
    }

    /// A version is read in its one format, 1: one naming any other is
    /// refused.
    #[test]
    fn a_version_of_another_format_is_refused() {
        for format in [1, 0, 2] {
            let id = format!(r#"{{"format":{format},"app":"app","object":"o","sequence":1,"epoch":0,"parent":"","snapshot":true}}"#);
            let mut tar = tar::Builder::new(vec![]);
            let mut header = tar::Header::new_gnu();
            header.set_size(id.len() as u64);
            header.set_mode(0o600);
            tar.append_data(&mut header, "nanovm.json", id.as_bytes()).unwrap();
            match committed::identity(&tar.into_inner().unwrap()[..]) {
                Ok(version) => assert_eq!(version.format, 1),
                Err(e) => assert!(format != 1 && e.kind() == ErrorKind::InvalidData, "{format}: {e}"),
            }
        }
    }

    /// A version can't reach outside the volume it is applied to, to list,
    /// remove, write or patch: not by `..`, nor through a symlink there, as
    /// each name is opened without following one (S4).
    #[test]
    fn a_version_cannot_write_outside_its_root() {
        let base = volume().parent().unwrap().to_path_buf();
        std::fs::write(base.join("victim"), "kept").unwrap();
        std::fs::create_dir(base.join("out")).unwrap();
        std::fs::write(base.join("out/v"), "kept").unwrap();
        let root = base.join("root");
        std::fs::create_dir(&root).unwrap();
        // None a version makes, but one in the root must not be followed.
        std::os::unix::fs::symlink(base.join("out"), root.join("l")).unwrap();
        let mut n = 0;
        let mut version = |changes: &str, entry: Option<&[u8]>| {
            let mut tar = tar::Builder::new(vec![]);
            let mut put = |name: &[u8], bytes: &[u8]| {
                let mut header = tar::Header::new_gnu();
                header.as_old_mut().name[..name.len()].copy_from_slice(name);
                header.set_size(bytes.len() as u64);
                header.set_mode(0o600);
                header.set_cksum();
                tar.append(&header, bytes).unwrap();
            };
            let id = r#"{"format":1,"app":"app","object":"o","sequence":2,"epoch":0,"parent":"","snapshot":false}"#;
            put(b"nanovm.json", id.as_bytes());
            put(b"changes.json", changes.as_bytes());
            if let Some(name) = entry {
                put(name, b"evil");
            }
            n += 1;
            let path = base.join(format!("evil{n}.tar"));
            std::fs::write(&path, tar.into_inner().unwrap()).unwrap();
            path
        };
        let empty = r#"{"listed":{},"whole":[],"patched":{}}"#;
        for (path, dots) in [
            (version(r#"{"listed":{"..":[]},"whole":[],"patched":{}}"#, None), true),
            (version(r#"{"listed":{},"whole":["../victim"],"patched":{}}"#, None), true),
            (version(empty, Some(b"data/../victim")), true),
            (version(r#"{"listed":{},"whole":[],"patched":{"../victim":[1,[[0,4]]]}}"#, Some(b"patch/../victim")), true),
            (version(r#"{"listed":{"l":[]},"whole":[],"patched":{}}"#, None), false),
            (version(r#"{"listed":{},"whole":["l/v"],"patched":{}}"#, None), false),
            (version(empty, Some(b"data/l/v")), false),
            (version(r#"{"listed":{},"whole":[],"patched":{"l/v":[4,[[0,4]]]}}"#, Some(b"patch/l/v")), false),
        ] {
            let e = committed::apply(&root, &path).err().expect("an unsafe version must be refused");
            if dots {
                assert_eq!(e.kind(), ErrorKind::InvalidData, "{e}");
            }
            assert_eq!(std::fs::read(base.join("victim")).unwrap(), b"kept");
            assert_eq!(std::fs::read(base.join("out/v")).unwrap(), b"kept", "{}: {e}", path.display());
        }
    }

    /// A version holds files and directories only. One with a symlink, a
    /// hard link or a device is refused, and nothing outside the root it is
    /// restored to changes. Before, a file under a symlink the version made
    /// was written through it, and a later change patched what a hard link
    /// named (S4).
    #[test]
    fn review_capture_symlink_cannot_escape_restore_root() {
        use tar::EntryType::{Char, Directory, Fifo, Link, Regular, Symlink};
        let base = volume().parent().unwrap().to_path_buf();
        let outside = base.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let victim = outside.join("victim");
        std::fs::write(&victim, "kept").unwrap();
        let (dir, file) = (outside.to_str().unwrap(), victim.to_str().unwrap());
        let patch = r#"{"listed":{},"whole":[],"patched":{"h":[4,[[0,4]]]}}"#;
        let cases: [(&str, &[(&str, &[(&str, tar::EntryType, &str)])]); 4] = [
            ("symlink", &[("", &[("data/l", Symlink, dir), ("data/l/victim", Regular, "evil")])]),
            ("hardlink", &[("", &[("data/h", Link, file)]), (patch, &[("patch/h", Regular, "evil")])]),
            ("char", &[("", &[("data/c", Char, "")])]),
            ("fifo", &[("", &[("data/f", Fifo, "")])]),
        ];
        for (name, versions) in cases {
            let history = base.join(name);
            std::fs::create_dir(&history).unwrap();
            let mut parent = String::new();
            for (i, (changes, entries)) in versions.iter().enumerate() {
                let mut tar = tar::Builder::new(vec![]);
                let mut add = |path: &str, kind: tar::EntryType, data: &str| {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(kind);
                    header.set_mode(0o644);
                    if matches!(kind, Link | Symlink) {
                        header.set_size(0);
                        tar.append_link(&mut header, path, data).unwrap();
                    } else {
                        header.set_size(data.len() as u64);
                        tar.append_data(&mut header, path, data.as_bytes()).unwrap();
                    }
                };
                let (n, snapshot) = (i + 1, changes.is_empty());
                let id = format!(r#"{{"format":1,"app":"app","object":"o","sequence":{n},"epoch":0,"parent":"{parent}","snapshot":{snapshot}}}"#);
                add("nanovm.json", Regular, &id);
                match snapshot {
                    true => add("data", Directory, ""),
                    false => add("changes.json", Regular, changes),
                }
                for &(path, kind, data) in *entries {
                    add(path, kind, data);
                }
                let path = history.join(format!("{n:020}.tar"));
                std::fs::write(&path, tar.into_inner().unwrap()).unwrap();
                parent = committed::digest(&path).unwrap();
            }
            let e = committed::restore(&history, &base.join(format!("root-{name}"))).err();
            assert_eq!(std::fs::read(&victim).unwrap(), b"kept", "{name}");
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1, "{name}");
            let e = e.expect(name);
            assert_eq!(e.kind(), ErrorKind::InvalidData, "{name}: {e}");
        }
    }

    /// A small change to a big file is captured as the bytes that changed.
    #[test]
    fn a_small_change_is_captured_small() {
        let cap = CaptureConfig::default();
        let v = volume();
        std::fs::write(v.join("big"), vec![1; 1 << 20]).unwrap();
        let mut j = captured(&v, &cap).unwrap();
        write(&mut j, &v, "big", 4096, b"X");
        j.commit().unwrap();
        write(&mut j, &v, "big", 1 << 19, b"Y");
        write(&mut j, &v, "big", 1 << 20, b"grown");
        j.commit().unwrap();
        let out = v.parent().unwrap().join("~committed/o");
        assert!(std::fs::metadata(out.join(format!("{:020}.tar", 1))).unwrap().len() > 1 << 20, "a snapshot first");
        let delta = std::fs::metadata(out.join(format!("{:020}.tar", 2))).unwrap().len();
        assert!(delta < 8192, "a two-write change took {delta} bytes");
        assert_eq!(archive_tree(&v, 2), tree(&v));
    }

    #[test]
    fn capture_process_cuts() {
        let cap = CaptureConfig::default();
        // Re-enter only this test in a fresh process. Exit bypasses every Drop;
        // these are process-loss cuts, not a simulation of power loss.
        if let Ok(v) = std::env::var("NANOVM_CAPTURE_CUT_ROOT") {
            let v = Path::new(&v);
            let cut = std::env::var("NANOVM_CAPTURE_CUT").unwrap().parse().unwrap();
            if std::env::var_os("NANOVM_CAPTURE_ROLLBACK").is_some() {
                EXIT_SYNC_AFTER.set(Some(cut));
                drop(captured(v, &cap).unwrap());
            } else {
                let mut j = captured(v, &cap).unwrap();
                change(&mut j, v);
                EXIT_SYNC_AFTER.set(Some(cut));
                j.commit().unwrap();
            }
            EXIT_SYNC_AFTER.set(None);
            return;
        }
        for rollback in [false, true] {
            let mut finished = false;
            for cut in 0..128 {
                let v = volume();
                let before = tree(&v);
                let mut j = captured(&v, &cap).unwrap();
                change(&mut j, &v);
                let changed = tree(&v);
                let out = committed::directory(&j.base, &j.dir);
                if rollback {
                    stage(&j, &cap);
                }
                drop(j);
                if !rollback { drop(captured(&v, &cap).unwrap()); }
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command.args(["--exact", "journal::tests::capture_process_cuts", "--nocapture"])
                    .env("NANOVM_CAPTURE_CUT_ROOT", &v).env("NANOVM_CAPTURE_CUT", cut.to_string());
                if rollback { command.env("NANOVM_CAPTURE_ROLLBACK", "1"); }
                let result = command.output().unwrap();
                assert!(result.status.success() || result.status.code() == Some(77),
                    "cut {cut}: {}", String::from_utf8_lossy(&result.stderr));
                let committed = !rollback && decided(&v);
                for _ in 0..2 {
                    drop(captured(&v, &cap).unwrap());
                    assert_eq!(tree(&v), if committed { &changed } else { &before }.clone(), "cut {cut}");
                    assert_eq!(published(&out), usize::from(committed));
                    if committed { assert_eq!(archive_tree(&v, 1), changed); }
                }
                std::fs::remove_dir_all(v.parent().unwrap()).unwrap();
                if result.status.success() {
                    assert!(cut >= 10, "did not reach capture boundaries");
                    println!("capture process loss: rollback={rollback}, {cut} abrupt exits recovered exactly");
                    finished = true;
                    break;
                }
            }
            assert!(finished, "process cut loop did not finish");
        }
    }

    /// A version records the epoch of the node's lease, and a node whose
    /// lease lapsed before the decision can't make it: the change is undone.
    #[test]
    fn commits_are_decided_only_under_the_lease() {
        use crate::lease::{Grant, Session, mono_ns};
        let session = Arc::new(Session::default());
        let mut obj = ObjShared::new("app".into(), "o");
        obj.leases = Some(session.clone());
        *obj.lease.lock() = Some(Grant { epoch: 7, session: 1, e_tag: None, version: None });
        let obj = Arc::new(obj);
        session.set(1, mono_ns() + 60_000_000_000);
        let (v, cap) = (volume(), CaptureConfig::default());
        let mut j = Journal::open(&v, 1 << 30, obj.clone(), Some(&cap)).unwrap();
        change(&mut j, &v);
        j.commit().unwrap();
        let mut archive = tar::Archive::new(File::open(v.parent().unwrap().join(format!("~committed/o/{:020}.tar", 1))).unwrap());
        let version: serde_json::Value = serde_json::from_reader(archive.entries().unwrap().next().unwrap().unwrap()).unwrap();
        assert_eq!((version["format"].as_u64(), version["epoch"].as_u64()), (Some(1), Some(7)));
        drop(j);
        // Without capture, the decision is the only check the journal makes.
        let w = volume();
        let before = tree(&w);
        let mut j = Journal::open(&w, 1 << 30, obj.clone(), None).unwrap();
        change(&mut j, &w);
        // The lease lapses: the node's session is over, a new one begun (a
        // session's deadline never moves back).
        session.set(2, mono_ns() + 60_000_000_000);
        assert!(j.commit().is_err());
        let x = volume();
        assert!(Journal::open(&x, 1 << 30, obj.clone(), None).unwrap().commit().is_err(),
            "nothing changed, but nothing may be published either");
        drop(j);
        drop(Journal::open(&w, 1 << 30, obj, None).unwrap());
        assert_eq!(tree(&w), before, "undone on recovery");
        for v in [v, w, x] {
            std::fs::remove_dir_all(v.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn capture_recovery_requires_a_durable_decision_before_publication() {
        let cap = CaptureConfig::default();
        let v = volume();
        let mut j = captured(&v, &cap).unwrap();
        change(&mut j, &v);
        let out = committed::directory(&j.base, &j.dir);
        stage(&j, &cap);
        // Model a new key visible to a new process, without a completed sync.
        j.log.as_ref().unwrap().write_all_at(&[7; 8], MAGIC.len() as u64).unwrap();
        drop(j);
        for _ in 0..2 {
            FAIL_SYNC_AFTER.set(Some(0));
            let result = captured(&v, &cap);
            FAIL_SYNC_AFTER.set(None);
            assert!(result.is_err());
            assert_eq!(published(&out), 0,
                "must not publish before the decision can be synced");
        }
        drop(captured(&v, &cap).unwrap());
        assert_eq!(archive_tree(&v, 1), tree(&v));
        std::fs::remove_dir_all(v.parent().unwrap()).unwrap();
    }

    #[test]
    fn capture_recovery_requires_durable_discard_before_undo() {
        let cap = CaptureConfig::default();
        let v = volume();
        let before = tree(&v);
        let mut j = captured(&v, &cap).unwrap();
        change(&mut j, &v);
        let changed = tree(&v);
        let out = committed::directory(&j.base, &j.dir);
        stage(&j, &cap);
        drop(j);
        for _ in 0..2 {
            FAIL_SYNC_AFTER.set(Some(0));
            let result = captured(&v, &cap);
            FAIL_SYNC_AFTER.set(None);
            assert!(result.is_err());
            assert_eq!(tree(&v), changed, "must persist discard before starting undo, including retries");
        }
        drop(captured(&v, &cap).unwrap());
        assert_eq!(tree(&v), before);
        assert_eq!(published(&out), 0);
        std::fs::remove_dir_all(v.parent().unwrap()).unwrap();
    }

    #[test]
    fn capture_and_local_commit_recover_as_one_decision() {
        let cap = CaptureConfig::default();
        for after in 0..64 {
            let v = volume();
            let before = tree(&v);
            let mut j = captured(&v, &cap).unwrap();
            change(&mut j, &v);
            let changed = tree(&v);
            FAIL_SYNC_AFTER.with(|c| c.set(Some(after)));
            let result = j.commit();
            FAIL_SYNC_AFTER.with(|c| c.set(None));
            let committed = decided(&v);
            let out = committed::directory(&j.base, &j.dir);
            drop(j);
            for _ in 0..2 {
                drop(captured(&v, &cap).unwrap());
                assert_eq!(tree(&v), if committed { changed.clone() } else { before.clone() }, "cut {after}");
                let count = published(&out);
                assert_eq!(count, usize::from(committed), "cut {after}: state/capture diverged");
                if committed { assert_eq!(archive_tree(&v, 1), changed, "cut {after}"); }
            }
            let mut next = captured(&v, &cap).unwrap();
            next.creating(&v.join("next")).unwrap();
            std::fs::write(v.join("next"), "next version").unwrap();
            next.commit().unwrap();
            assert_eq!(archive_tree(&v, if committed { 2 } else { 1 }), tree(&v));
            if committed { assert_eq!(archive_tree(&v, 1), changed, "older capture changed"); }
            std::fs::remove_dir_all(v.parent().unwrap()).unwrap();
            if result.is_ok() {
                assert!(after >= 10, "did not exercise capture/publication sync boundaries");
                println!("capture commit: {after} sync-error cuts preserve state/version agreement");
                return;
            }
        }
        panic!("capture commit fault loop did not finish");
    }

    #[test]
    fn rejected_capture_is_never_published_by_interrupted_recovery() {
        let cap = CaptureConfig::default();
        for after in 0..128 {
            let v = volume();
            let before = tree(&v);
            let mut j = captured(&v, &cap).unwrap();
            change(&mut j, &v);
            let out = committed::directory(&j.base, &j.dir);
            stage(&j, &cap);
            drop(j); // Prepared, but the commit decision was never made.
            FAIL_SYNC_AFTER.with(|c| c.set(Some(after)));
            let result = captured(&v, &cap);
            FAIL_SYNC_AFTER.with(|c| c.set(None));
            let finished = result.is_ok();
            drop(result);
            for _ in 0..2 {
                drop(captured(&v, &cap).unwrap());
                assert_eq!(tree(&v), before, "recovery cut {after}");
                assert_eq!(published(&out), 0, "published rejected version at cut {after}");
                // Left staged, a later recovery would take it for decided.
                assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0, "kept rejected version at cut {after}");
            }
            std::fs::remove_dir_all(v.parent().unwrap()).unwrap();
            if finished {
                assert!(after > 5);
                println!("capture rollback: {after} recovery sync-error cuts exclude rejected versions");
                return;
            }
        }
        panic!("capture recovery fault loop did not finish");
    }

    #[test]
    fn capture_caps_reject_without_discarding_accepted_history() {
        {
            let v = volume();
            // Snapshots only, so the byte limit binds.
            let cap = CaptureConfig { max_mb: 1, snapshot_every: 1, ..Default::default() };
            std::fs::write(v.join("large"), vec![7; 600_000]).unwrap();
            let mut j = captured(&v, &cap).unwrap();
            write(&mut j, &v, "a", 0, b"accepted");
            j.commit().unwrap();
            let accepted = tree(&v);
            write(&mut j, &v, "a", 0, b"rejected");
            assert_eq!(j.commit().unwrap_err().kind(), ErrorKind::StorageFull);
            assert!(j.commit().is_err(), "failed capture must poison commit retries");
            drop(j);
            let mut j = captured(&v, &cap).unwrap();
            assert_eq!(tree(&v), accepted);
            assert_eq!(archive_tree(&v, 1), accepted);
            j.commit().unwrap(); // Read-only acceptance does not consume history.
            drop(j);
            assert!(reopen(&v, 1 << 30).is_err(), "disabling capture must not create silent gaps");
            std::fs::remove_dir_all(v.parent().unwrap()).unwrap();
        }
    }

    /// Writes as the filesystem host does: journal first, then the write.
    fn write(j: &mut Journal, v: &Path, name: &str, at: u64, data: &[u8]) {
        let file = File::options().read(true).write(true).open(v.join(name)).unwrap();
        j.writing(&file, at, at + data.len() as u64).unwrap();
        file.write_all_at(data, at).unwrap();
    }

    /// Renames as the filesystem host does: the rename is recorded and a
    /// file in the way goes into the journal, then the rename is made.
    fn rename(j: &mut Journal, v: &Path, from: &str, to: &str) {
        let (from, to) = (v.join(from), v.join(to));
        let replaced = std::fs::symlink_metadata(&to).ok();
        let (_, slot) = j.renaming(&from, &to, replaced.as_ref()).unwrap();
        std::fs::rename(&from, &to).unwrap();
        j.renamed(&from, &to);
        if let Some(slot) = slot {
            j.discard_created(&slot, replaced.unwrap().len()).unwrap();
        }
    }

    /// Whether the volume's last commit decided: no record in its log is
    /// signed with the key there.
    fn decided(v: &Path) -> bool {
        let log = std::fs::read(v.parent().unwrap().join("~journal/o/log")).unwrap();
        decode(&log, log[8..16].try_into().unwrap()).is_empty()
    }

    /// The key the log's records are signed with now.
    fn signed_with(j: &Journal) -> Vec<u8> {
        std::fs::read(j.dir.join(LOG)).unwrap()[8..16].to_vec()
    }

    #[test]
    fn a_path_under_what_became_a_file_commits() {
        let v = volume();
        let mut j = open(&v);
        j.creating(&v.join("n/m")).unwrap();
        j.creating(&v.join("n")).unwrap();
        std::fs::write(v.join("n"), "x").unwrap();
        j.commit().unwrap();
        assert_eq!(std::fs::read(v.join("n")).unwrap(), b"x");
        open(&v).check().unwrap();
    }

    #[test]
    fn ancestor_sync_failure_prevents_initialization() {
        let v = volume();
        let levels = v.canonicalize().unwrap().ancestors().count();
        for cut in 0..levels {
            FAIL_SYNC_AFTER.set(Some(cut as u32));
            let result = sync_ancestors(&v);
            FAIL_SYNC_AFTER.set(None);
            assert!(result.is_err(), "sync failure at ancestor {cut} must propagate");
        }
        sync_ancestors(&v).unwrap();
    }

    thread_local! {
        /// Undos recovery may apply before a simulated crash.
        static CRASH_AFTER: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
        static FAIL_SYNC_AFTER: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
        static EXIT_SYNC_AFTER: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
        /// Syncs so far.
        static SYNCS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    /// Milliseconds each sync takes on top of the disk's time, on any thread.
    static SLOW_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    pub(super) fn sync_point() -> std::io::Result<()> {
        SYNCS.set(SYNCS.get() + 1);
        std::thread::sleep(std::time::Duration::from_millis(SLOW_MS.load(std::sync::atomic::Ordering::Relaxed)));
        EXIT_SYNC_AFTER.with(|c| match c.get() {
            Some(0) => std::process::exit(77),
            n => c.set(n.map(|n| n - 1)),
        });
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
        let left: Vec<_> = std::fs::read_dir(&j.dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, [LOG], "the journal holds only its log");
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
                let key = (phase == "commit").then(|| signed_with(&j));
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
                // Once the key is replaced, a failed commit sync has an
                // ambiguous durable outcome; on this running kernel it is new.
                let expected = if key.is_some_and(|k| k != signed_with(&j)) { &changed } else { &before };
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
            j.append(Undo::Created(PathBuf::from("o/a"))).unwrap();
            j.log.as_ref().unwrap().set_len(j.end - 1).unwrap();
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

    /// A directory descriptor resolves to where the directory is now, even
    /// after it moved; the volume's own descriptor resolves to the volume.
    #[test]
    fn directory_descriptors_resolve_where_they_are_now() {
        let v = volume();
        let j = open(&v);
        let (root, d) = (File::open(&v).unwrap(), File::open(v.join("d")).unwrap());
        std::fs::rename(v.join("d"), v.join("moved")).unwrap();
        assert_eq!(j.local_at(&root, "x"), Some(v.join("x")));
        assert_eq!(j.local_at(&d, "x"), Some(v.join("moved/x")));
        assert_eq!(j.local_at(&d, "../../other"), None);
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

    /// A create that can't succeed where the journal can name it (under a
    /// file, a name over NAME_MAX, a path reaching PATH_MAX) is refused, not
    /// recorded: a record of it used to fail every later recovery, so the
    /// object answered 503 `instantiate-failed` for good (N-502, N-503).
    #[test]
    fn creates_the_journal_cannot_name_are_refused() {
        let v = volume();
        let mut j = open(&v);
        let deep: PathBuf = std::iter::repeat_n("n".repeat(255), 16).collect();
        let long = "n".repeat(256);
        for (p, kind) in [("a/x", ErrorKind::NotADirectory), ("f/x/y", ErrorKind::NotADirectory),
            (long.as_str(), ErrorKind::InvalidFilename), (deep.to_str().unwrap(), ErrorKind::InvalidFilename)] {
            assert_eq!(j.creating(&v.join(p)).unwrap_err().kind(), kind, "{p}");
            assert_eq!(j.removing_dir(&v.join(p)).unwrap_err().kind(), kind, "{p}");
        }
        assert!(j.log.is_none());
        // Nor may a rename put what is under a directory past PATH_MAX: a
        // chain that fits, moved under a directory deep enough that it
        // wouldn't. The chain's files are walked too.
        let levels = |n| std::iter::repeat_n("n".repeat(255), n).collect::<PathBuf>();
        std::fs::create_dir_all(v.join("c").join(levels(7))).unwrap();
        std::fs::write(v.join("c").join(levels(7)).join("n".repeat(255)), b"x").unwrap();
        std::fs::create_dir_all(v.join(levels(8))).unwrap();
        let too_deep = j.renaming(&v.join("c"), &v.join(levels(8)).join("c"), None);
        assert_eq!(too_deep.unwrap_err().kind(), ErrorKind::InvalidFilename);
        assert!(j.log.is_none());
        j.renaming(&v.join("c"), &v.join(levels(7)).join("c"), None).unwrap();
        // The walk never follows a symlink: an operator's link to an
        // ancestor (guests can't make one) would otherwise never end.
        std::fs::create_dir(v.join("s")).unwrap();
        std::os::unix::fs::symlink("..", v.join("s/up")).unwrap();
        std::os::unix::fs::symlink(v.join("s"), v.join("link")).unwrap();
        j.renaming(&v.join("s"), &v.join("s2"), None).unwrap();
        j.renaming(&v.join("link"), &v.join("link2"), None).unwrap();
    }

    /// What a request leaves in the log of operations that can't succeed
    /// recovers: a create under a missing directory, then a rename to under
    /// a file, under a name past NAME_MAX or under a missing directory,
    /// whose record is synced but whose cancellation never is (a crash, or a
    /// failed write, between the two). Such a path names nothing: there is
    /// nothing to undo there, nor a directory to sync.
    #[test]
    fn records_of_operations_that_cannot_succeed_recover() {
        let long = "n".repeat(256);
        for to in ["f/x/y".to_string(), format!("{long}/x"), "missing/x".into()] {
            let v = volume();
            let before = tree(&v);
            let mut j = open(&v);
            j.creating(&v.join("missing/x")).unwrap();
            // As `QuotaGuard::rename` asks: nothing can be at `to` to replace.
            assert!(std::fs::symlink_metadata(v.join(&to)).is_err());
            j.renaming(&v.join("a"), &v.join(&to), None).unwrap();
            drop(j);
            reopen(&v, 1 << 30).unwrap_or_else(|e| panic!("{}: recovery failed: {e}", &to[to.len() - 3..]));
            assert_eq!(tree(&v), before);
            std::fs::remove_dir_all(v.parent().unwrap()).unwrap();
        }
    }

    /// The log's paths are relative to the data root, which can move deeper
    /// between a crash and the recovery after it (a backup restored into a
    /// longer root): an entry whose whole path fitted is then past PATH_MAX,
    /// while each of its names fits. Recovery undoes each kind of record
    /// there through descriptors, rather than taking it for nothing there
    /// and dropping the undo (N-690).
    #[test]
    fn long_paths_after_the_root_moves_are_undone() {
        use std::os::fd::AsRawFd;
        let v = volume();
        let base = v.parent().unwrap().to_path_buf();
        let name = |c: &str| c.repeat(200);
        // A directory whose entries' whole paths fit, by 300 bytes or more.
        let mut deep = v.clone();
        while deep.as_os_str().len() + 256 + 201 + 300 < libc::PATH_MAX as usize {
            deep.push("c".repeat(255));
        }
        std::fs::create_dir_all(&deep).unwrap();
        for (c, data) in [("f", "hello world"), ("g", "gee"), ("h", "aitch")] {
            std::fs::write(deep.join(name(c)), data).unwrap();
        }
        std::fs::create_dir(deep.join(name("e"))).unwrap();
        let dir = File::open(&deep).unwrap();
        // Short paths for the directory and what is in it, wherever it is.
        let listing = || tree(Path::new(&format!("/proc/self/fd/{}", dir.as_raw_fd())));
        let before = listing();
        // A change of each kind, recorded as the filesystem host records it.
        let at = |c: &str| deep.join(name(c));
        let mut j = open(&v);
        j.creating(&at("n")).unwrap();
        std::fs::write(at("n"), "new").unwrap();
        j.created(&at("n")).unwrap();
        j.creating(&at("m")).unwrap();
        std::fs::create_dir(at("m")).unwrap();
        let f = File::options().write(true).open(at("f")).unwrap();
        j.writing(&f, 0, 21).unwrap();
        f.write_all_at(b"HELLO WORLD, AND MORE", 0).unwrap();
        j.removing_dir(&at("e")).unwrap();
        std::fs::remove_dir(at("e")).unwrap();
        j.unlink(&at("g"), 3).unwrap();
        j.renaming(&at("h"), &at("i"), None).unwrap();
        std::fs::rename(at("h"), at("i")).unwrap();
        j.renamed(&at("h"), &at("i"));
        assert_ne!(listing(), before);
        drop(j);
        // The root moves deeper, past what those whole paths can take.
        let mut moved = base.clone().into_os_string();
        moved.push("-moved");
        let moved = PathBuf::from(moved);
        let root = moved.join("m".repeat(255)).join("m".repeat(255)).join("m".repeat(255)).join("root");
        std::fs::create_dir_all(root.parent().unwrap()).unwrap();
        std::fs::rename(&base, &root).unwrap();
        let now = root.join("o").join(deep.strip_prefix(&v).unwrap()).join(name("n"));
        assert!(now.as_os_str().len() >= libc::PATH_MAX as usize);
        open(&root.join("o"));
        assert_eq!(listing(), before);
        std::fs::remove_dir_all(moved).unwrap();
    }

    /// The parent of `p` (whatever the length of `p`) and `p`'s name, for
    /// the `*at` calls a guest's descriptors make.
    fn at(p: &Path) -> Option<(File, std::ffi::CString)> {
        use std::os::unix::ffi::OsStrExt;
        let parent = File::open(p.parent()?).ok()?;
        Some((parent, std::ffi::CString::new(p.file_name()?.as_bytes()).ok()?))
    }

    /// Random path shapes (files as parents, 255- and 256-byte names, a
    /// chain of directories ending just short of PATH_MAX and past it,
    /// missing parents) through creates, mkdirs, overwrites, unlinks, rmdirs
    /// and renames, driving the journal as `quota.rs` does and making the
    /// syscalls as a guest's descriptors would; then the request fails. The
    /// next open always recovers, to the tree as it was.
    #[test]
    fn random_path_shapes_always_roll_back() {
        use std::os::fd::AsRawFd;
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut pick = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let n = |c: &str, len| c.repeat(len);
        for round in 0..300 {
            let v = volume();
            let mut chain = v.clone();
            while chain.as_os_str().len() + 2 * 256 < libc::PATH_MAX as usize {
                chain.push(n("c", 255));
            }
            std::fs::create_dir_all(&chain).unwrap();
            let deep = chain.strip_prefix(&v).unwrap().to_path_buf();
            let shapes: Vec<PathBuf> = vec![
                "a".into(), "a/x".into(), "d".into(), "d/b".into(), "d/x".into(), "e".into(), "e/x".into(),
                "f".into(), "m/x".into(), n("n", 255).into(), n("n", 256).into(), PathBuf::from("d").join(n("o", 255)),
                deep.join("x"), deep.join(n("p", 255)), deep.join(n("p", 255)).join(n("q", 255)),
                deep.join(n("p", 255)).join("x"),
            ];
            let before = tree(&v);
            let mut j = open(&v);
            let mut ops = vec![];
            for _ in 0..8 {
                let (p, q) = (v.join(&shapes[pick(shapes.len())]), v.join(&shapes[pick(shapes.len())]));
                let op = pick(6);
                let short = |p: &Path| {
                    let c = p.strip_prefix(&v).unwrap().iter().map(|c| c.to_string_lossy().into_owned());
                    c.map(|c| if c.len() > 8 { format!("{}*{}", &c[..1], c.len()) } else { c }).collect::<Vec<_>>().join("/")
                };
                ops.push((op, short(&p), short(&q)));
                match op {
                    0 | 1 if j.creating(&p).is_ok() => {
                        // `open_at`: only a file that wasn't there is new.
                        let existed = std::fs::symlink_metadata(&p).is_ok();
                        let Some((dir, name)) = at(&p) else { continue };
                        // SAFETY: plain *at calls on a descriptor we own.
                        let made = unsafe {
                            if op == 1 {
                                libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o755) == 0
                            } else {
                                let fd = libc::openat(dir.as_raw_fd(), name.as_ptr(), libc::O_CREAT | libc::O_WRONLY | libc::O_CLOEXEC, 0o644);
                                fd >= 0 && libc::close(fd) == 0
                            }
                        };
                        if made && op == 0 && !existed {
                            let _ = j.created(&p);
                        }
                    }
                    2 => {
                        if let Ok(f) = File::options().write(true).open(&p)
                            && j.writing(&f, 0, 4).is_ok()
                        {
                            f.write_all_at(b"XXXX", 0).unwrap();
                        }
                    }
                    3 => match std::fs::symlink_metadata(&p) {
                        Ok(md) if md.is_file() => _ = j.unlink(&p, md.len()),
                        _ => _ = std::fs::remove_file(&p),
                    },
                    4 if j.removing_dir(&p).is_ok() => _ = std::fs::remove_dir(&p),
                    5 if p != q => {
                        // `QuotaGuard::rename`.
                        let Ok(src) = std::fs::symlink_metadata(&p) else { continue };
                        let replaced = std::fs::symlink_metadata(&q).ok();
                        if replaced.as_ref().is_some_and(|d| d.is_file()) && src.is_dir() {
                            continue;
                        }
                        let Ok((record, slot)) = j.renaming(&p, &q, replaced.as_ref()) else { continue };
                        let file = slot.map(|slot| (slot, replaced.as_ref().unwrap().len()));
                        let moved = match (at(&p), at(&q)) {
                            // SAFETY: as above.
                            (Some((a, x)), Some((b, y))) => unsafe {
                                libc::renameat(a.as_raw_fd(), x.as_ptr(), b.as_raw_fd(), y.as_ptr()) == 0
                            },
                            _ => false,
                        };
                        if moved {
                            j.renamed(&p, &q);
                            if let Some((slot, len)) = file {
                                j.discard_created(&slot, len).unwrap();
                            }
                            continue;
                        }
                        if let Some((slot, len)) = file {
                            j.restore(&slot, &q, len).unwrap();
                        }
                        j.cancel_rename(record).unwrap();
                    }
                    _ => {}
                }
            }
            drop(j);
            reopen(&v, 1 << 30).unwrap_or_else(|e| panic!("round {round}: recovery failed ({e}) after {ops:?}"));
            let after = tree(&v);
            let differ: Vec<_> = before.keys().chain(after.keys()).filter(|k| before.get(*k) != after.get(*k))
                .map(|k| (k.iter().map(|c| c.len().min(9)).sum::<usize>(), k.file_name().map(|n| n.len()), after.get(k).cloned())).collect();
            assert!(differ.is_empty(), "round {round}: {ops:?}: differ (short length, name length, now) {differ:?}");
            std::fs::remove_dir_all(v.parent().unwrap()).unwrap();
        }
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
        let (at, slot) = j.renaming(&v.join("a"), &to, Some(&std::fs::metadata(&to).unwrap())).unwrap();
        let slot = slot.unwrap();
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
            let (at, slot) = j.renaming(&from, &to, Some(&std::fs::metadata(&to).unwrap())).unwrap();
            let slot = slot.unwrap();
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

    /// What lies in the log past the current request's records is left from
    /// earlier requests, or was never written by the journal: none of it is
    /// undone, whether signed with an old key or a guessed one.
    #[test]
    fn only_the_current_requests_records_are_undone() {
        let v = volume();
        let mut j = open(&v);
        change(&mut j, &v);
        let old = j.key;
        j.commit().unwrap();
        let committed = tree(&v);
        // A record a guest might plant: replayed, it would empty a file.
        let forged = Undo::Size(PathBuf::from("o/d/a2"), 0);
        for key in [old, [0; 8]] {
            write(&mut j, &v, "d/a2", 0, b"X");
            if key == old {
                let stale = std::fs::metadata(j.dir.join(LOG)).unwrap().len();
                assert!(stale > j.end + 200, "earlier records lie past this one");
            }
            j.log.as_ref().unwrap().write_all_at(&encode(&key, &forged), j.end).unwrap();
            drop(j);
            j = open(&v);
            assert_eq!(tree(&v), committed);
        }
    }

    /// A log whose first block isn't the tag and a key is refused, not taken
    /// as empty: rolling nothing back would keep what a crash left half done.
    #[test]
    fn a_log_without_its_tag_is_refused() {
        let v = volume();
        let log = v.parent().unwrap().join("~journal/o/log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        // Records from the start, with no tag or key before them.
        let old = [&[1, 0, 0, 0, 0, 0, 0, 0, 2, 3, 0, 0, 0, 0, 0, 0, 0][..], b"o/a"].concat();
        std::fs::write(&log, &old).unwrap();
        let e = reopen(&v, 1 << 30).err().expect("an unknown log must be refused");
        assert_eq!(e.kind(), ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&log).unwrap(), old);
    }

    /// A log is whole whenever it is there: one cut short while being made
    /// is left under another name, which recovery clears.
    #[test]
    fn a_log_cut_short_while_made_is_ignored() {
        let v = volume();
        let before = tree(&v);
        let mut j = open(&v);
        FAIL_SYNC_AFTER.set(Some(0));
        let f = File::options().write(true).open(v.join("a")).unwrap();
        assert!(j.writing(&f, 0, 1).is_err());
        FAIL_SYNC_AFTER.set(None);
        drop(j);
        assert!(!v.parent().unwrap().join("~journal/o/log").exists());
        let mut j = open(&v);
        assert_eq!(tree(&v), before);
        write(&mut j, &v, "a", 0, b"H");
        j.commit().unwrap();
        let left: Vec<_> = std::fs::read_dir(&j.dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, [LOG]);
    }

    /// The log keeps the space a big request needed only up to `KEEP`.
    #[test]
    fn the_log_shrinks_after_a_big_request() {
        let v = volume();
        std::fs::write(v.join("big"), vec![1; 200 << 10]).unwrap();
        let mut j = open(&v);
        write(&mut j, &v, "big", 0, &vec![2; 200 << 10]);
        j.commit().unwrap();
        assert_eq!(std::fs::metadata(j.dir.join(LOG)).unwrap().len(), KEEP);
        write(&mut j, &v, "big", 0, b"3");
        drop(j);
        open(&v);
        assert_eq!(std::fs::read(v.join("big")).unwrap(), vec![2; 200 << 10]);
    }

    /// What a request pays in syncs once its object has a log: one per
    /// change that destroys something, however many records it takes
    /// (renaming over a file takes two, overwriting a file's start two),
    /// then one per changed path and one for the key.
    #[test]
    fn records_made_together_share_a_sync() {
        let v = volume();
        let mut j = open(&v);
        write(&mut j, &v, "f", 0, b"F");
        j.commit().unwrap();
        let syncs = |j: &mut Journal, f: &dyn Fn(&mut Journal)| {
            SYNCS.set(0);
            f(j);
            SYNCS.get()
        };
        assert_eq!(syncs(&mut j, &|j| write(j, &v, "a", 0, b"HELLO")), 1);
        assert_eq!(syncs(&mut j, &|j| j.creating(&v.join("n")).unwrap()), 1);
        std::fs::write(v.join("n"), "new").unwrap();
        j.created(&v.join("n")).unwrap();
        assert_eq!(syncs(&mut j, &|j| rename(j, &v, "n", "f")), 1);
        // `a`, `f` and the volume, then the key.
        assert_eq!(syncs(&mut j, &|j| j.commit().unwrap()), 4);
    }

    /// A commit waits for the disk on a thread of its own: the runtime's
    /// other tasks go on meanwhile, even with one worker thread.
    #[test]
    fn other_tasks_run_while_a_commit_waits() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_time().build().unwrap();
        rt.block_on(async {
            let ticks = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let counter = ticks.clone();
            tokio::spawn(async move {
                loop {
                    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            });
            tokio::spawn(async {
                let v = volume();
                let obj = Arc::new(ObjShared::new("app".into(), "o"));
                let limits = crate::config::Limits::default();
                let quota = crate::quota::QuotaGuard::new(&v, &limits, Default::default(), obj, None).unwrap();
                quota.journal(|j| j.creating(&v.join("n"))).unwrap();
                SLOW_MS.store(100, std::sync::atomic::Ordering::Relaxed);
                quota.commit().await.unwrap();
                SLOW_MS.store(0, std::sync::atomic::Ordering::Relaxed);
            })
            .await
            .unwrap();
            let n = ticks.load(std::sync::atomic::Ordering::Relaxed);
            assert!(n > 100, "the other task ran {n} times");
        });
    }

    /// A commit syncs the paths it changed many at a time, and fails if any
    /// of them can't be synced, whichever thread meets it.
    #[test]
    fn a_commit_syncs_its_paths_together() {
        for bad in [None].into_iter().chain((0..8).map(|i| Some(i * 7))) {
            let v = volume();
            let before = tree(&v);
            let mut j = open(&v);
            for i in 0..64 {
                let p = v.join(format!("n{i}"));
                j.creating(&p).unwrap();
                std::fs::write(&p, "x").unwrap();
                j.created(&p).unwrap();
            }
            if let Some(i) = bad {
                // A file a commit can't sync, as on a failing disk.
                std::fs::remove_file(v.join(format!("n{i}"))).unwrap();
                std::os::unix::fs::symlink(format!("n{i}"), v.join(format!("n{i}"))).unwrap();
            }
            SLOW_MS.store(if bad.is_none() { 50 } else { 0 }, std::sync::atomic::Ordering::Relaxed);
            let t0 = std::time::Instant::now();
            let result = j.commit();
            SLOW_MS.store(0, std::sync::atomic::Ordering::Relaxed);
            if bad.is_none() {
                result.unwrap();
                assert!(t0.elapsed() < std::time::Duration::from_millis(1600), "65 syncs of 50 ms took {:?}", t0.elapsed());
            } else {
                assert!(result.is_err(), "bad path {bad:?}");
                drop(j);
                open(&v);
                assert_eq!(tree(&v), before);
            }
            std::fs::remove_dir_all(v.parent().unwrap()).unwrap();
        }
    }

    /// A commit with nothing to commit stays on the object's task: it doesn't
    /// wait for a blocking thread, here the only one, which is busy.
    #[test]
    fn a_commit_of_nothing_stays_on_its_task() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).max_blocking_threads(1).enable_time().build().unwrap();
        rt.block_on(async {
            let v = volume();
            let obj = Arc::new(ObjShared::new("app".into(), "o"));
            let quota = crate::quota::QuotaGuard::new(&v, &Default::default(), Default::default(), obj, None).unwrap();
            let busy = tokio::task::spawn_blocking(|| std::thread::sleep(std::time::Duration::from_millis(500)));
            let commit = tokio::time::timeout(std::time::Duration::from_millis(200), quota.commit());
            commit.await.expect("not waiting for a blocking thread").unwrap();
            busy.await.unwrap();
        });
    }

    /// A node that lost an object's lease can't ship the version it
    /// committed before losing it, while the new owner's versions ship;
    /// restoring from the store on a third root gives the new owner's history
    /// (W-90).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_node_that_lost_the_lease_cannot_ship() {
        use crate::lease::{Leases, Session, mono_ns};
        use crate::ship::{Objects, Store};
        let leases: Arc<dyn object_store::ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let store = Objects(Arc::new(object_store::memory::InMemory::new()));
        let cap = CaptureConfig::default();
        // A node (its leases lasting `ttl`) takes the object for its copy
        // under `objects` as its supervisor does (the claim restores what
        // the store has); and its object's view of the lease.
        let take = async |name: &str, ttl: std::time::Duration, objects: &Path| {
            let node = Leases::new(leases.clone(), name, ttl);
            let (grant, (id, until)) = loop {
                let session = node.renew().await;
                match node.acquire("app/o").await {
                    Ok(grant) => break (grant, session),
                    Err(_) => tokio::time::sleep(ttl / 8).await,
                }
            };
            crate::ship::claim(&store, name, grant.epoch, "app", "o", objects).await.unwrap();
            node.hold("app/o", grant.clone());
            let session = Arc::new(Session::default());
            session.set(id, until.max(mono_ns() + 60_000_000_000));
            let mut obj = ObjShared::new("app".into(), "o");
            (obj.leases, *obj.lease.get_mut()) = (Some(session), Some(grant));
            (node, Arc::new(obj))
        };
        let ship = async |node: &Leases, v: &Path| {
            let dir = v.parent().unwrap().join("~committed/o");
            crate::ship::ship_object(&store, Some(node), "app", "o", &dir).await
        };
        let va = volume();
        let (a, a_obj) = take("a", std::time::Duration::from_secs(1), va.parent().unwrap()).await;
        let mut j = Journal::open(&va, 1 << 30, a_obj.clone(), Some(&cap)).unwrap();
        write(&mut j, &va, "a", 0, b"first");
        j.commit().unwrap();
        a.renew().await;
        ship(&a, &va).await.unwrap();
        // Committed under A's lease, but not shipped before B takes it.
        write(&mut j, &va, "a", 0, b"late!");
        j.commit().unwrap();
        drop(j);
        let root = |name: &str| std::env::temp_dir().join(format!("nanovm-w90-{name}-{}", std::process::id()));
        let vb = root("b").join("o");
        let _ = std::fs::remove_dir_all(vb.parent().unwrap());
        std::fs::create_dir_all(vb.parent().unwrap()).unwrap();
        let (b, b_obj) = take("b", std::time::Duration::from_secs(10), vb.parent().unwrap()).await;
        assert_eq!(ship(&a, &va).await.unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
        let vb = vb.canonicalize().unwrap();
        let mut j = Journal::open(&vb, 1 << 30, b_obj, Some(&cap)).unwrap();
        write(&mut j, &vb, "a", 0, b"B's");
        j.commit().unwrap();
        ship(&b, &vb).await.unwrap();
        assert!(ship(&a, &va).await.is_err(), "still refused");
        let (head, _) = crate::ship::head(&store, "app", "o").await.unwrap();
        assert_eq!((head.sequence, head.node.as_str()), (2, "b"));
        let version = store.get(&format!("app/o/{:020}-{}.tar", 2, head.digest)).await.unwrap().unwrap();
        assert_eq!(crate::committed::identity(&version[..]).unwrap().epoch, 2, "the store's second version is B's");
        let third = root("c").join("objects/app");
        let _ = std::fs::remove_dir_all(root("c"));
        std::fs::create_dir_all(&third).unwrap();
        assert_eq!(crate::ship::restore(&store, "app", "o", &third, None, 0).await.unwrap().version, 2);
        assert_eq!(tree(&third.join("o")), tree(&vb));
        assert_eq!(std::fs::read(third.join("o/a")).unwrap(), b"B'sst world");
        for d in [va.parent().unwrap(), vb.parent().unwrap(), &root("c")] {
            std::fs::remove_dir_all(d).unwrap();
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
        let mut j = reopen(&v, 130).unwrap();
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
            let payload: usize = decode(&std::fs::read(j.dir.join(LOG)).unwrap(), &j.key)
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
            let count = decode(&std::fs::read(j.dir.join(LOG)).unwrap(), &j.key).len();
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
