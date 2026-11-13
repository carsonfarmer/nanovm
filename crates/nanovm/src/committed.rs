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

// No independent mutable cursor: the contiguous immutable filenames are the
// head. Returns the last sequence, and how many versions and bytes are here:
// a shipper removes those it shipped but the last, maybe meanwhile.
fn head(dir: &Path) -> io::Result<(u64, u64, u64)> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok((0, 0, 0)),
        Err(e) => return Err(e),
    };
    let (mut count, mut first, mut last, mut bytes) = (0u64, u64::MAX, 0u64, 0u64);
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_str().is_some_and(|n| n.ends_with(".staged")) {
            continue;
        }
        let sequence = name
            .to_str()
            .and_then(|s| s.strip_suffix(".tar"))
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| invalid("invalid committed version name"))?;
        if sequence == 0 || name != filename(sequence).as_str() || !entry.file_type()?.is_file() {
            return Err(invalid("invalid committed version entry"));
        }
        let len = match entry.metadata() {
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            len => len?.len(),
        };
        count += 1;
        (first, last) = (first.min(sequence), last.max(sequence));
        bytes = bytes.checked_add(len).ok_or_else(|| invalid("capture size overflow"))?;
    }
    if count > 0 && count != last - first + 1 {
        return Err(invalid("gap in committed versions"));
    }
    Ok((last, count, bytes))
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

/// Stages the version a commit of `volume` makes, before the journal
/// replaces its key `key`: a snapshot, or `changes` to the last version.
pub(super) fn prepare(
    volume: &Path,
    out: &Path,
    obj: &ObjShared,
    cap: &CaptureConfig,
    key: &[u8; 8],
    changes: &Changes,
) -> io::Result<()> {
    let (last, count, used) = head(out)?;
    let limit = cap
        .max_mb
        .checked_mul(1 << 20)
        .ok_or_else(|| invalid("capture limit overflow"))?;
    if count >= cap.max_versions || used >= limit {
        return Err(ErrorKind::StorageFull.into());
    }
    let version = Version {
        format: FORMAT,
        app: obj.app.to_string(),
        object: obj.id.to_string(),
        sequence: last
            .checked_add(1)
            .ok_or_else(|| invalid("capture sequence overflow"))?,
        epoch: obj.lease_epoch()?,
        parent: if last == 0 {
            String::new()
        } else {
            digest(&out.join(filename(last)))?
        },
        snapshot: last % cap.snapshot_every.max(1) == 0,
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
    if !version.snapshot {
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
    let next = head(out)?.0 + 1;
    fs::rename(out.join(format!("{next:020}{}", decided_by(key))), out.join(filename(next)))?;
    Ok(next)
}

/// Settles what commits left staged when their instance stopped, given the
/// journal's key `key`: a version staged under it never decided and is
/// discarded. When the journal has records to roll back (`undecided`), the
/// discard is durable first, even one an earlier recovery made: a second
/// recovery must not mistake a rejected version for a commit. Any other
/// decided, and is published once `decided` has made the decision durable.
pub(super) fn settle(
    out: &Path,
    obj: &ObjShared,
    key: &[u8; 8],
    undecided: bool,
    decided: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let mut staged = vec![];
    match fs::read_dir(out) {
        Ok(entries) => {
            for entry in entries {
                let name = entry?.file_name().to_string_lossy().into_owned();
                if name.ends_with(".staged") {
                    staged.push(name);
                }
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }
    staged.sort();
    if let Some(i) = staged.iter().position(|n| n.ends_with(&decided_by(key))) {
        fs::remove_file(out.join(staged.remove(i)))?;
    }
    if undecided {
        sync(out)?;
    }
    if staged.is_empty() {
        return Ok(());
    }
    // A decision seen here may not be durable yet: make it so before any
    // capture becomes visible to a receiver.
    decided()?;
    for name in staged {
        let (last, ..) = head(out)?;
        let path = out.join(&name);
        let version = read_version(&mut tar::Archive::new(File::open(&path)?).entries()?)?;
        let parent = if last == 0 { String::new() } else { digest(&out.join(filename(last)))? };
        if version.app != *obj.app || version.object != *obj.id || version.sequence != last + 1 || version.parent != parent {
            return Err(invalid("out-of-order committed version"));
        }
        fs::rename(&path, out.join(filename(version.sequence)))?;
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

const DIR: OFlags = OFlags::RDONLY.union(OFlags::DIRECTORY).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);

/// The directory `path` names in `root`, opened a name at a time and never
/// through a symlink, as journal recovery reaches entries, so nothing it
/// reaches is outside; made where missing if `make`.
fn dir_at(root: &OwnedFd, path: &Path, make: bool) -> io::Result<OwnedFd> {
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
fn remove_at(dir: &OwnedFd, name: &OsStr) -> io::Result<()> {
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
/// each change after it, checked against the version before. Returns the
/// last version's sequence.
pub fn restore(dir: &Path, root: &Path) -> io::Result<u64> {
    let mut versions = vec![];
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name();
        let n = name.to_str().and_then(|n| n.strip_suffix(".tar")?.parse::<u64>().ok());
        versions.push(n.filter(|&n| name == filename(n).as_str()).ok_or_else(|| invalid("invalid committed version name"))?);
    }
    versions.sort();
    let mut snapshot = None;
    for &n in versions.iter().rev() {
        if read_version(&mut tar::Archive::new(File::open(dir.join(filename(n)))?).entries()?)?.snapshot {
            snapshot = Some(n);
            break;
        }
    }
    let snapshot = snapshot.ok_or_else(|| invalid("no snapshot"))?;
    fs::create_dir(root)?;
    let mut parent = None;
    for &n in versions.iter().filter(|&&n| n >= snapshot) {
        let path = dir.join(filename(n));
        // Checked before it is applied.
        let version = read_version(&mut tar::Archive::new(File::open(&path)?).entries()?)?;
        if version.sequence != n || parent.as_ref().is_some_and(|p| *p != version.parent) {
            return Err(invalid("broken chain of committed versions"));
        }
        apply(root, &path)?;
        parent = Some(digest(&path)?);
    }
    Ok(snapshot.max(*versions.last().expect("a snapshot is one")))
}
