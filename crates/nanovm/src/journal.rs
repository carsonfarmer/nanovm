//! A request's changes to its object's volume take effect together or not at
//! all, and survive a crash once taken: an undo journal, as SQLite keeps.
//!
//! Before each change, what it would destroy is recorded and synced:
//! - a file's size, the first time it is written or truncated;
//! - the bytes a write or a truncation overwrites;
//! - "remove this", for a new file or directory;
//! - "rename it back", for a rename; a removed or replaced file is itself
//!   renamed into the journal rather than deleted;
//! - "recreate this", for a removed directory.
//!
//! Writes then happen in place, so open files stay valid. Committing syncs
//! what changed and deletes the log. Rolling back replays it backwards, which
//! is what [`recover`] does when an instance starts, so a failed request (its
//! instance is restarted) and a crash take the same path. Every step is
//! idempotent, so a crash during recovery is recovered from too.
//!
//! The log lives beside the volume, at `<objects>/~journal/<object>`, which
//! no object id can name. Paths in it are relative to `<objects>`, inside the
//! volume or the journal (a file the guest still writes after unlinking it
//! lives there).

use std::collections::HashSet;
use std::fs::File;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::FileExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::object::ObjShared;

const LOG: &str = "log";

enum Undo {
    Size(PathBuf, u64),
    Bytes(PathBuf, u64, Vec<u8>),
    Created(PathBuf),
    RemovedDir(PathBuf),
    /// The entry at the first path was renamed to the second.
    Renamed(PathBuf, PathBuf),
}

pub struct Journal {
    /// `<objects>`, which every path in the log is relative to.
    base: PathBuf,
    volume: PathBuf,
    dir: PathBuf,
    /// Open while the current request has changes.
    log: Option<File>,
    /// Files whose size is recorded, and everything to sync on commit.
    sized: HashSet<PathBuf>,
    touched: HashSet<PathBuf>,
    slots: u64,
    /// Waits for the disk aren't charged to its CPU budget.
    obj: Arc<ObjShared>,
}

impl Journal {
    /// The journal of `volume`, after rolling back what a previous instance
    /// left uncommitted.
    pub fn open(volume: &Path, obj: Arc<ObjShared>) -> std::io::Result<Journal> {
        let base = volume.parent().expect("a volume has a parent").to_path_buf();
        let dir = base.join("~journal").join(volume.file_name().expect("a volume has a name"));
        recover(&base, &dir)?;
        Ok(Journal {
            base,
            volume: volume.to_path_buf(),
            dir,
            log: None,
            sized: HashSet::new(),
            touched: HashSet::new(),
            slots: 0,
            obj,
        })
    }

    /// `path` relative to the base, if it is in the volume or the journal.
    /// No symlink can exist there, so resolving `..` by hand is exact.
    pub fn local(&self, path: &Path) -> Option<PathBuf> {
        let mut out = PathBuf::new();
        for c in path.components() {
            match c {
                Component::ParentDir => _ = out.pop(),
                Component::Normal(n) => out.push(n),
                _ => {}
            }
        }
        let out = Path::new("/").join(out);
        let inside = out.starts_with(&self.volume) || out.starts_with(&self.dir);
        inside.then(|| out.strip_prefix(&self.base).expect("inside the base").to_path_buf())
    }

    /// The path, relative to the base, of the file behind `file`.
    pub fn local_file(&self, file: &File) -> Option<PathBuf> {
        self.local(&fd_path(file)?)
    }

    pub fn abs(&self, p: &Path) -> PathBuf {
        self.base.join(p)
    }

    /// Appends `undo` and syncs it, before the change it undoes is made.
    fn record(&mut self, undo: Undo) -> std::io::Result<()> {
        if self.log.is_none() {
            std::fs::create_dir_all(&self.dir)?;
            let log = File::options().create(true).append(true).open(self.dir.join(LOG))?;
            sync(&self.dir)?;
            self.log = Some(log);
        }
        let log = self.log.as_mut().expect("opened above");
        log.write_all(&encode(&undo))?;
        let t0 = Instant::now();
        log.sync_data()?;
        self.obj.uncharge(t0.elapsed());
        Ok(())
    }

    /// Before `file` (at `p`) is written over `[at, end)`: its size, the first
    /// time, and the bytes the write replaces. The bytes the log holds.
    pub fn writing(&mut self, p: &Path, file: &File, at: u64, end: u64) -> std::io::Result<u64> {
        let len = file.metadata()?.len();
        if self.sized.insert(p.to_path_buf()) {
            self.record(Undo::Size(p.to_path_buf(), len))?;
            self.touched.insert(p.to_path_buf());
        }
        let end = end.min(len);
        if at >= end {
            return Ok(0);
        }
        let mut old = vec![0; (end - at) as usize];
        file.read_exact_at(&mut old, at)?;
        let n = old.len() as u64;
        self.record(Undo::Bytes(p.to_path_buf(), at, old))?;
        Ok(n)
    }

    /// Before the entry `p` is created.
    pub fn creating(&mut self, p: &Path) -> std::io::Result<()> {
        self.record(Undo::Created(p.to_path_buf()))?;
        self.touched.insert(p.to_path_buf());
        self.touch_parent(p);
        Ok(())
    }

    /// Before the empty directory `p` is removed.
    pub fn removing_dir(&mut self, p: &Path) -> std::io::Result<()> {
        self.record(Undo::RemovedDir(p.to_path_buf()))?;
        self.touch_parent(p);
        Ok(())
    }

    /// Removes the entry `p` by moving it into the journal.
    pub fn remove(&mut self, p: &Path) -> std::io::Result<()> {
        self.slots += 1;
        let slot = self.local(&self.dir.join(self.slots.to_string())).expect("the journal is local");
        self.record(Undo::Renamed(p.to_path_buf(), slot.clone()))?;
        std::fs::rename(self.abs(p), self.abs(&slot))?;
        self.touch_parent(p);
        Ok(())
    }

    /// Renames `from` to `to`, moving whatever `to` held into the journal.
    pub fn rename(&mut self, from: &Path, to: &Path) -> std::io::Result<()> {
        if std::fs::symlink_metadata(self.abs(to)).is_ok() {
            self.remove(to)?;
        }
        self.record(Undo::Renamed(from.to_path_buf(), to.to_path_buf()))?;
        std::fs::rename(self.abs(from), self.abs(to))?;
        // What is to be synced moved with it.
        let moved = |p: PathBuf| match p.strip_prefix(from) {
            Ok(rest) if rest.as_os_str().is_empty() => to.to_path_buf(),
            Ok(rest) => to.join(rest),
            Err(_) => p,
        };
        self.touched = self.touched.drain().map(moved).collect();
        self.touch_parent(from);
        self.touch_parent(to);
        Ok(())
    }

    fn touch_parent(&mut self, p: &Path) {
        self.touched.insert(p.parent().unwrap_or(Path::new("")).to_path_buf());
    }

    /// Makes the request's changes durable and forgets how to undo them:
    /// whether there were any.
    pub fn commit(&mut self) -> std::io::Result<bool> {
        let Some(log) = self.log.take() else { return Ok(false) };
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
        Ok(true)
    }
}

/// Rolls back the log in `dir` if there is one, then empties `dir`.
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
        let gone = |r: std::io::Result<()>| match r {
            Err(e) if e.kind() != ErrorKind::NotFound && e.kind() != ErrorKind::AlreadyExists => Err(e),
            _ => Ok(()),
        };
        match &undo {
            Undo::Size(p, n) => gone(File::options().write(true).open(abs(p)).and_then(|f| f.set_len(*n)))?,
            Undo::Bytes(p, at, data) => gone(File::options().write(true).open(abs(p)).and_then(|f| f.write_all_at(data, *at)))?,
            Undo::Created(p) => gone(std::fs::remove_file(abs(p)).or_else(|_| std::fs::remove_dir(abs(p))))?,
            Undo::RemovedDir(p) => gone(std::fs::create_dir(abs(p)))?,
            Undo::Renamed(from, to) if !abs(from).exists() => gone(std::fs::rename(abs(to), abs(from)))?,
            Undo::Renamed(..) => {}
        }
        let p = match &undo {
            Undo::Size(p, _) | Undo::Bytes(p, ..) => p.as_path(),
            Undo::Created(p) | Undo::RemovedDir(p) | Undo::Renamed(p, _) => {
                p.parent().unwrap_or(Path::new(""))
            }
        };
        sync_if_there(&base.join(p))?;
        #[cfg(test)]
        tests::crash_point()?;
        file.set_len(at)?;
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
    let mut out = vec![];
    let next = |log: &mut &[u8]| -> Option<Undo> {
        Some(match bytes(log)?[..] {
            [0] => Undo::Size(path(log)?, num(log)?),
            [1] => Undo::Bytes(path(log)?, num(log)?, bytes(log)?),
            [2] => Undo::Created(path(log)?),
            [3] => Undo::RemovedDir(path(log)?),
            [4] => Undo::Renamed(path(log)?, path(log)?),
            _ => return None,
        })
    };
    loop {
        let at = (len - log.len()) as u64;
        match next(&mut log) {
            Some(u) => out.push((at, u)),
            None => return out,
        }
    }
}

/// Removes everything in `dir` (slots left by a commit or a rollback).
fn clear(dir: &Path) -> std::io::Result<()> {
    match std::fs::read_dir(dir) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(entries) => entries.flatten().try_for_each(|e| {
            let p = e.path();
            if e.file_type()?.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) }
        }),
    }
}

/// Syncs a file or a directory.
fn sync(p: &Path) -> std::io::Result<()> {
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
pub fn fd_path(file: &File) -> Option<PathBuf> {
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
        v
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

    fn open(v: &Path) -> Journal {
        Journal::open(v, Arc::new(ObjShared::new("app".into(), "o"))).unwrap()
    }

    /// Writes as the filesystem host does: journal first, then the write.
    fn write(j: &mut Journal, v: &Path, name: &str, at: u64, data: &[u8]) {
        let file = File::options().read(true).write(true).open(v.join(name)).unwrap();
        let p = j.local_file(&file).unwrap();
        j.writing(&p, &file, at, at + data.len() as u64).unwrap();
        file.write_all_at(data, at).unwrap();
    }

    thread_local! {
        /// Undos recovery may apply before a simulated crash.
        static CRASH_AFTER: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
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
        // Paths relative to the base; the volume is `o`.
        let l = |name: &str| Path::new("o").join(name);
        write(j, v, "a", 6, b"WORLD, again");
        let f = File::options().read(true).write(true).open(v.join("f")).unwrap();
        j.writing(&l("f"), &f, 0, 2).unwrap();
        f.set_len(0).unwrap();
        j.creating(&l("n")).unwrap();
        std::fs::write(v.join("n"), "new").unwrap();
        j.creating(&l("m")).unwrap();
        std::fs::create_dir(v.join("m")).unwrap();
        j.remove(&l("d/b")).unwrap();
        j.removing_dir(&l("e")).unwrap();
        std::fs::remove_dir(v.join("e")).unwrap();
        j.rename(&l("a"), &l("d/a2")).unwrap();
        j.rename(&l("n"), &l("f")).unwrap();
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
        assert!(j.commit().unwrap());
        assert!(!j.commit().unwrap(), "nothing left to commit");
        open(&v);
        assert_eq!(tree(&v), after);
        assert_eq!(std::fs::read(v.join("d/a2")).unwrap(), b"hello WORLD, again");
        assert_eq!(std::fs::read(v.join("f")).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(&j.dir).unwrap().count(), 0, "the journal is empty");
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
            let crashed = Journal::open(&v, Arc::new(ObjShared::new("app".into(), "o"))).is_err();
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
        j.remove(&j.local(&v.join("a")).unwrap()).unwrap();
        let p = j.local_file(&file).expect("the journal is local");
        assert!(p.starts_with("~journal"), "{p:?}");
        j.writing(&p, &file, 0, 5).unwrap();
        file.write_all_at(b"HELLO", 0).unwrap();
        drop(j);
        open(&v);
        assert_eq!(tree(&v), before);
    }

    /// Only the volume and its journal are journaled, and `..` can't leave.
    #[test]
    fn only_the_volume_is_local() {
        let v = volume();
        let j = open(&v);
        assert_eq!(j.local(&v.join("d/../a")), Some(PathBuf::from("o/a")));
        assert_eq!(j.local(&v.join("../other/x")), None);
        assert_eq!(j.local(&v.join("../../../etc/passwd")), None);
        assert_eq!(j.local(Path::new("/tmp/x")), None);
    }
}
