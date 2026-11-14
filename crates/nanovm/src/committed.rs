//! Immutable versions of an object's committed volume, staged under the
//! journal's commit decision: the whole volume every `snapshot_every`
//! versions, otherwise what one commit changed against the version before.
//! The app's existing storage lock owns this tree; no distributed fence is
//! implied.
//!
//! `committed/<sequence>.tar`, in the object's directory, is a published
//! version. One being
//! staged is `<sequence>.<decision>.staged` beside them, where `<decision>`
//! names the journal key its commit replaces: recovery discards it while
//! that is still the journal's key, and publishes it once it isn't.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, FileTimes};
use std::io::{self, ErrorKind, Read, Write};
use std::ops::Range;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, fchmod, mkdirat, openat, statat, unlinkat};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{config::CaptureConfig, journal::sync, object::ObjShared};

/// The format a version names in its `nanovm.json`; no other is read.
const FORMAT: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Version {
    pub format: u32,
    pub app: String,
    pub object: String,
    pub sequence: u64,
    /// The epoch of the node's lease it was committed under (W-10).
    pub epoch: u64,
    /// SHA-256 of the version before, or empty for the first.
    pub parent: String,
    /// The whole volume, not a change to the version before.
    pub snapshot: bool,
}

/// What a commit changed in its volume; paths are relative to it.
#[derive(Default)]
pub(crate) struct Changes {
    /// Directories whose entries changed.
    pub listed: Vec<PathBuf>,
    /// Files and directory trees taken whole.
    pub whole: Vec<PathBuf>,
    /// Files changed in place, with the ranges of them that changed.
    pub patched: Vec<(PathBuf, Vec<Range<u64>>)>,
}

/// How a version that is a change records it: each listed directory's
/// entries (and whether each is a directory), whole entries (under `data/`)
/// and each patched file's length and changed ranges (bytes under `patch/`).
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    listed: BTreeMap<String, BTreeSet<(String, bool)>>,
    whole: BTreeSet<String>,
    patched: BTreeMap<String, (u64, Vec<(u64, u64)>)>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

/// Where the versions of the object whose directory is `base` are.
pub fn directory(base: &Path) -> PathBuf {
    base.join("committed")
}

fn filename(sequence: u64) -> String {
    format!("{sequence:020}.tar")
}

/// What a version staged under journal key `key` ends its name with.
fn decided_by(key: &[u8; 8]) -> String {
    format!(".{}.staged", &format!("{:x}", Sha256::digest(key))[..16])
}

pub fn digest(path: &Path) -> io::Result<String> {
    let mut hash = Sha256::new();
    io::copy(&mut File::open(path)?, &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

/// The versions published in `dir` (`<sequence>.tar`), in order, each with
/// its size, and the names of those staged, in order; none if `dir` isn't
/// there. No independent mutable cursor: the last name is the head. A
/// shipper removes those it shipped but the last, maybe meanwhile, in any
/// order, so this may see some of them and not others (B-503).
pub(crate) fn list(dir: &Path) -> io::Result<(Vec<(u64, u64)>, Vec<String>)> {
    let (mut versions, mut staged) = (vec![], vec![]);
    let entries = match fs::read_dir(dir) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok((versions, staged)),
        entries => entries?,
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".staged") {
            staged.push(name);
        } else if let Some(n) = name.strip_suffix(".tar").and_then(|n| n.parse().ok()).filter(|&n| name == filename(n)) {
            match entry.metadata() {
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                md => versions.push((n, md?.len())),
            }
        }
    }
    versions.sort();
    staged.sort();
    Ok((versions, staged))
}

/// The last version published in `dir` (0: none).
fn last(dir: &Path) -> io::Result<u64> {
    Ok(list(dir)?.0.last().map_or(0, |v| v.0))
}

struct Bounded {
    file: File,
    remaining: u64,
}

impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(ErrorKind::StorageFull.into());
        }
        let n = self.file.write(bytes)?;
        self.remaining -= n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn name(path: &Path) -> io::Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| invalid("non-UTF-8 path"))
}

/// Stages the version a commit of `volume` under lease epoch `epoch`
/// makes, before the journal replaces its key `key`: `changes` to the last
/// version, or with none (or when one is due) a snapshot. What waits here
/// is bounded in bytes (`max_mb`), not in versions, so a store that is down
/// for a while doesn't stop writes; nor does a snapshot fall due while
/// more than `snapshot_every` versions wait, so what waits grows with what
/// is written, not with the volume (B-501).
pub(super) fn prepare(
    volume: &Path,
    out: &Path,
    obj: &ObjShared,
    epoch: u64,
    cap: &CaptureConfig,
    key: &[u8; 8],
    changes: Option<&Changes>,
) -> io::Result<()> {
    let (versions, _) = list(out)?;
    let (last, used) = (versions.last().map_or(0, |v| v.0), versions.iter().map(|v| v.1).sum::<u64>());
    let limit = cap.max_mb.saturating_mul(1 << 20);
    if used >= limit {
        return Err(ErrorKind::StorageFull.into());
    }
    let every = cap.snapshot_every.max(1);
    let version = Version {
        format: FORMAT,
        app: obj.app.to_string(),
        object: obj.id.to_string(),
        sequence: last + 1,
        epoch,
        parent: if last == 0 {
            String::new()
        } else {
            digest(&out.join(filename(last)))?
        },
        snapshot: changes.is_none() || (last % every == 0 && versions.len() as u64 <= every),
    };
    let metadata = serde_json::to_vec(&version)?;
    if !out.try_exists()? {
        fs::create_dir_all(out)?;
        // Persist the archive directory's name before a commit can depend on it.
        for dir in [out, out.parent().expect("in the object's directory")] {
            sync(dir)?;
        }
    }
    let staged = out.join(format!("{:020}{}", version.sequence, decided_by(key)));
    let writer = Bounded {
        file: File::options().write(true).create_new(true).open(&staged)?,
        remaining: limit - used,
    };
    let mut archive = tar::Builder::new(writer);
    archive.follow_symlinks(false);
    archive.sparse(false);
    let mut append = |name: &str, bytes: &[u8]| {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o600);
        archive.append_data(&mut header, name, bytes)
    };
    append("nanovm.json", &metadata)?;
    let mut whole = vec![PathBuf::new()];
    if let Some(changes) = changes.filter(|_| !version.snapshot) {
        let mut manifest = Manifest::default();
        for dir in &changes.listed {
            let mut entries = BTreeSet::new();
            for entry in fs::read_dir(volume.join(dir))? {
                let entry = entry?;
                entries.insert((name(Path::new(&entry.file_name()))?, entry.file_type()?.is_dir()));
            }
            manifest.listed.insert(name(dir)?, entries);
        }
        for (path, ranges) in &changes.patched {
            let len = fs::metadata(volume.join(path))?.len();
            let ranges = ranges.iter().map(|r| (r.start, r.end - r.start)).collect();
            manifest.patched.insert(name(path)?, (len, ranges));
        }
        manifest.whole = changes.whole.iter().map(|p| name(p)).collect::<io::Result<_>>()?;
        append("changes.json", &serde_json::to_vec(&manifest)?)?;
        for (path, (_, ranges)) in &manifest.patched {
            let file = File::open(volume.join(path))?;
            let mut bytes = vec![];
            for &(at, len) in ranges {
                let start = bytes.len();
                bytes.resize(start + len as usize, 0);
                file.read_exact_at(&mut bytes[start..], at)?;
            }
            append(&format!("patch/{path}"), &bytes)?;
        }
        whole = changes.whole.clone();
    }
    // Directories before what is in them, so a reader can make each in turn.
    while let Some(path) = whole.pop() {
        let source = volume.join(&path);
        let kind = fs::symlink_metadata(&source)?.file_type();
        if !kind.is_dir() && !kind.is_file() {
            return Err(invalid("unsupported capture file type"));
        }
        archive.append_path_with_name(&source, Path::new("data").join(&path))?;
        if kind.is_dir() {
            for entry in fs::read_dir(&source)? {
                whole.push(path.join(entry?.file_name()));
            }
        }
    }
    drop(archive.into_inner()?);
    sync(&staged)?;
    sync(out)
}

/// Publishes the version staged under journal key `key`, which the journal
/// has replaced, and returns its sequence. Recovery does it again if the
/// rename is lost.
pub(super) fn publish(out: &Path, key: &[u8; 8]) -> io::Result<u64> {
    let next = last(out)? + 1;
    fs::rename(out.join(format!("{next:020}{}", decided_by(key))), out.join(filename(next)))?;
    Ok(next)
}

/// Settles what commits left staged when their instance stopped, given the
/// journal's key `key`: a version staged under it never decided and is
/// discarded. When the journal has records to roll back (`undecided`), the
/// discard is durable first, even one an earlier recovery made: a second
/// recovery must not mistake a rejected version for a commit. Any other
/// decided, and is published, under the sequence `prepare` named it with,
/// once `decided` has made the decision durable.
pub(super) fn settle(out: &Path, key: &[u8; 8], undecided: bool, decided: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    let (_, mut staged) = list(out)?;
    if let Some(i) = staged.iter().position(|n| n.ends_with(&decided_by(key))) {
        fs::remove_file(out.join(staged.remove(i)))?;
    }
    if undecided && out.exists() {
        sync(out)?;
    }
    if staged.is_empty() {
        return Ok(());
    }
    // A decision seen here may not be durable yet: make it so before any
    // capture becomes visible to a receiver.
    decided()?;
    for name in staged {
        let sequence = name.split('.').next().unwrap_or_default();
        fs::rename(out.join(&name), out.join(format!("{sequence}.tar")))?;
    }
    sync(out)
}

/// What the version `archive` reads says of itself.
pub fn identity(archive: impl Read) -> io::Result<Version> {
    read_version(&mut tar::Archive::new(archive).entries()?)
}

fn read_version<R: Read>(entries: &mut tar::Entries<R>) -> io::Result<Version> {
    let mut entry = entries.next().ok_or_else(|| invalid("missing capture identity"))??;
    if entry.path()? != Path::new("nanovm.json") || entry.size() > 4096 {
        return Err(invalid("invalid capture identity"));
    }
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes)?;
    let version: Version = serde_json::from_slice(&bytes)?;
    if version.format != FORMAT || version.sequence == 0 {
        return Err(invalid("wrong capture identity"));
    }
    Ok(version)
}

/// `path`, which a version names, if each part of it is a name.
fn names(path: &Path) -> io::Result<&Path> {
    match path.components().all(|c| matches!(c, Component::Normal(_))) {
        true => Ok(path),
        false => Err(invalid("unsafe path in a committed version")),
    }
}

pub(crate) const DIR: OFlags = OFlags::RDONLY.union(OFlags::DIRECTORY).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);

/// The directory `path` names in `root`, opened a name at a time and never
/// through a symlink, as journal recovery reaches entries, so nothing it
/// reaches is outside; made where missing if `make`.
pub(crate) fn dir_at(root: &OwnedFd, path: &Path, make: bool) -> io::Result<OwnedFd> {
    let mut dir = openat(root, ".", DIR, Mode::empty())?;
    for name in names(path)? {
        if make && let Err(e) = mkdirat(&dir, name, Mode::from_raw_mode(0o777)) && e != Errno::EXIST {
            return Err(e.into());
        }
        dir = openat(&dir, name, DIR, Mode::empty())?;
    }
    Ok(dir)
}

/// The directory holding what `path` names in `root`, as [`dir_at`] opens
/// it, and its name there.
fn parent_at<'a>(root: &OwnedFd, path: &'a Path) -> io::Result<(OwnedFd, &'a OsStr)> {
    let name = names(path)?.file_name().ok_or_else(|| invalid("unsafe path in a committed version"))?;
    Ok((dir_at(root, path.parent().expect("named"), false)?, name))
}

/// The names in `dir`, each with whether it is a directory.
fn listing(dir: &OwnedFd) -> io::Result<Vec<(OsString, bool)>> {
    let mut out = vec![];
    for entry in rustix::fs::Dir::read_from(dir)? {
        let entry = entry?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        let kind = match entry.file_type() {
            FileType::Unknown => FileType::from_raw_mode(statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?.st_mode),
            kind => kind,
        };
        if name != "." && name != ".." {
            out.push((name.to_owned(), kind == FileType::Directory));
        }
    }
    Ok(out)
}

/// Removes `name` from `dir`, and all in it, never through a symlink.
pub(crate) fn remove_at(dir: &OwnedFd, name: &OsStr) -> io::Result<()> {
    match statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Err(Errno::NOENT) => Ok(()),
        Ok(st) if FileType::from_raw_mode(st.st_mode) == FileType::Directory => {
            let inner = openat(dir, name, DIR, Mode::empty())?;
            for (name, _) in listing(&inner)? {
                remove_at(&inner, &name)?;
            }
            Ok(unlinkat(dir, name, AtFlags::REMOVEDIR)?)
        }
        Ok(_) => Ok(unlinkat(dir, name, AtFlags::empty())?),
        Err(e) => Err(e.into()),
    }
}

/// Applies the version in `file` to the volume at `root`: a snapshot to an
/// empty directory, a change to what the version before left. Returns it.
/// It holds files and directories only, and everything it names is reached
/// through `root`'s descriptor as [`dir_at`] does (S4).
pub fn apply(root: &Path, file: &Path) -> io::Result<Version> {
    let mut archive = tar::Archive::new(File::open(file)?);
    let mut entries = archive.entries()?;
    let version = read_version(&mut entries)?;
    let root = openat(CWD, root, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
    let mut manifest = Manifest::default();
    if !version.snapshot {
        let mut entry = entries.next().ok_or_else(|| invalid("missing changes"))??;
        if entry.path()? != Path::new("changes.json") {
            return Err(invalid("missing changes"));
        }
        manifest = serde_json::from_reader(&mut entry)?;
        for (dir, keep) in &manifest.listed {
            let dir = dir_at(&root, Path::new(dir), true)?;
            for (entry, is_dir) in listing(&dir)? {
                if !keep.contains(&(name(Path::new(&entry))?, is_dir)) {
                    remove_at(&dir, &entry)?;
                }
            }
        }
        for path in &manifest.whole {
            match parent_at(&root, Path::new(path)) {
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                at => at.and_then(|(dir, name)| remove_at(&dir, name))?,
            }
        }
    }
    for entry in entries {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let (kind, mode, mtime) = (entry.header().entry_type(), entry.header().mode()? & 0o777, entry.header().mtime()?);
        let mode = Mode::from_raw_mode(mode);
        if !kind.is_file() && !(kind.is_dir() && path.starts_with("data")) {
            return Err(invalid("unsupported entry type in a committed version"));
        }
        if let Ok(rest) = path.strip_prefix("data") {
            if kind.is_dir() {
                fchmod(dir_at(&root, rest, true)?, mode)?;
                continue;
            }
            // A new file, not one already there under another name.
            let (dir, name) = parent_at(&root, rest)?;
            if let Err(e) = unlinkat(&dir, name, AtFlags::empty()) && e != Errno::NOENT {
                return Err(e.into());
            }
            let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let mut file = File::from(openat(&dir, name, flags, mode)?);
            io::copy(&mut entry, &mut file)?;
            fchmod(&file, mode)?;
            let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(mtime.max(1));
            file.set_times(FileTimes::new().set_accessed(mtime).set_modified(mtime))?;
        } else if let Some((rest, (len, ranges))) =
            path.strip_prefix("patch").ok().and_then(|p| Some((p, manifest.patched.get(p.to_str()?)?)))
        {
            let (dir, name) = parent_at(&root, rest)?;
            let file = File::from(openat(&dir, name, OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?);
            let md = file.metadata()?;
            if !md.is_file() || md.nlink() != 1 {
                return Err(invalid("a patched file that is not one file"));
            }
            file.set_len(*len)?;
            for &(at, n) in ranges {
                let mut bytes = vec![0; n as usize];
                entry.read_exact(&mut bytes)?;
                file.write_all_at(&bytes, at)?;
            }
        } else {
            return Err(invalid("unexpected entry in a committed version"));
        }
    }
    Ok(version)
}

/// Rebuilds at `root`, which must not exist yet, the volume that the
/// versions in `dir` (`<sequence>.tar`) end with: the last snapshot, then
/// each change after it. They are a chain, each the parent its successor
/// names: `ship::restore` fetches them so, and checks each. Returns the last
/// version's sequence.
pub fn restore(dir: &Path, root: &Path) -> io::Result<u64> {
    let versions: Vec<u64> = list(dir)?.0.into_iter().map(|(n, _)| n).collect();
    let mut snapshot = None;
    for &n in versions.iter().rev() {
        if identity(File::open(dir.join(filename(n)))?)?.snapshot {
            snapshot = Some(n);
            break;
        }
    }
    let snapshot = snapshot.ok_or_else(|| invalid("no snapshot"))?;
    fs::create_dir(root)?;
    for &n in versions.iter().filter(|&&n| n >= snapshot) {
        apply(root, &dir.join(filename(n)))?;
    }
    Ok(snapshot.max(*versions.last().expect("a snapshot is one")))
}
