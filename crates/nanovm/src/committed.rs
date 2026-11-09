//! Immutable namespace and changed-byte captures under the journal's commit decision.
//! The app's existing storage lock owns this tree; no distributed fence is implied.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{config::CaptureConfig, journal::sync, object::ObjShared};

const STAGED: &str = "capture";

pub(super) struct Capture {
    cap: CaptureConfig,
    out: PathBuf,
    initialized: bool,
    sequence: u64,
    used: u64,
    digest: String,
    // Only same-path, same-inode files can inherit bytes. Renames are copied.
    // Rebind after journal recovery, including restoration onto different inodes.
    baseline: HashMap<PathBuf, u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Inherit {
    length: u64,
}

impl Capture {
    pub(super) fn open(volume: &Path, journal: &Path, obj: &ObjShared, cap: &CaptureConfig) -> io::Result<Self> {
        let out = directory(volume.parent().expect("inside app"), journal);
        let (last, used) = head(&out)?;
        let baseline = if last == 0 {
            HashMap::new()
        } else {
            let version = read_version(File::open(out.join(filename(last)))?, obj)?;
            if version.sequence != last {
                return Err(invalid("wrong capture sequence"));
            }
            files(volume)?
        };
        Ok(Self {
            cap: cap.clone(),
            digest: if last == 0 {
                String::new()
            } else {
                digest(&out.join(filename(last)))?
            },
            out,
            initialized: false,
            sequence: last,
            used,
            baseline,
        })
    }

    pub(super) fn prepare(
        &mut self,
        volume: &Path,
        journal: &Path,
        obj: &ObjShared,
        changed: impl Fn(u64, u64) -> Vec<Range<u64>>,
    ) -> io::Result<()> {
        if !self.initialized {
            fs::create_dir_all(&self.out)?;
            // Re-establish this after every open, including a failed earlier
            // initialization. A visible directory need not be durable yet.
            for dir in [
                &self.out,
                self.out.parent().expect("inside ~committed"),
                volume.parent().expect("inside app"),
            ] {
                sync(dir)?;
            }
            self.initialized = true;
        }
        prepare(self, volume, journal, obj, changed)
    }

    // Journal::commit has already synced the log deletion. Recovery cannot
    // use this path: it must first establish its own decision barrier.
    pub(super) fn publish(&self, journal: &Path) -> io::Result<()> {
        // This activation prepared the version; only recovery needs to reread
        // metadata and validate an interrupted publication against history.
        fs::hard_link(journal.join(STAGED), self.out.join(filename(self.sequence)))?;
        sync(&self.out)?;
        fs::remove_file(journal.join(STAGED))
    }
}

fn files(volume: &Path) -> io::Result<HashMap<PathBuf, u64>> {
    let mut files = HashMap::new();
    let mut pending = vec![volume.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            } else {
                files.insert(entry.path(), entry.metadata()?.ino());
            }
        }
    }
    Ok(files)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Version {
    format: u32,
    app: String,
    object: String,
    sequence: u64,
    parent: String,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

pub(super) fn directory(base: &Path, journal: &Path) -> PathBuf {
    base.join("~committed")
        .join(journal.file_name().expect("journal has an object name"))
}

fn filename(sequence: u64) -> String {
    format!("{sequence:020}.tar")
}

fn digest(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

// No independent mutable cursor: the contiguous immutable filenames are the head.
fn head(dir: &Path) -> io::Result<(u64, u64)> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok((0, 0)),
        Err(e) => return Err(e),
    };
    let (mut count, mut last, mut bytes) = (0u64, 0u64, 0u64);
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let sequence = name
            .to_str()
            .and_then(|s| s.strip_suffix(".tar"))
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| invalid("invalid committed version name"))?;
        if sequence == 0 || name != filename(sequence).as_str() || !entry.file_type()?.is_file() {
            return Err(invalid("invalid committed version entry"));
        }
        count += 1;
        last = last.max(sequence);
        bytes = bytes
            .checked_add(entry.metadata()?.len())
            .ok_or_else(|| invalid("capture size overflow"))?;
    }
    if count != last {
        return Err(invalid("gap in committed versions"));
    }
    Ok((last, bytes))
}

struct Bounded {
    file: File,
    remaining: u64,
    hash: Sha256,
}

impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(ErrorKind::StorageFull.into());
        }
        let n = self.file.write(bytes)?;
        self.remaining -= n as u64;
        self.hash.update(&bytes[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn prepare(
    capture: &mut Capture,
    volume: &Path,
    journal: &Path,
    obj: &ObjShared,
    changed: impl Fn(u64, u64) -> Vec<Range<u64>>,
) -> io::Result<()> {
    let limit = capture
        .cap
        .max_mb
        .checked_mul(1 << 20)
        .ok_or_else(|| invalid("capture limit overflow"))?;
    if capture.sequence >= capture.cap.max_versions || capture.used >= limit {
        return Err(ErrorKind::StorageFull.into());
    }
    let version = Version {
        format: 2,
        app: obj.app.to_string(),
        object: obj.id.to_string(),
        sequence: capture
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("capture sequence overflow"))?,
        parent: capture.digest.clone(),
    };
    let metadata = serde_json::to_vec(&version)?;
    if metadata.len() > 4096 {
        return Err(invalid("capture identity too large"));
    }
    let staged = journal.join(STAGED);
    let writer = Bounded {
        file: File::options().write(true).create_new(true).open(&staged)?,
        remaining: limit - capture.used,
        hash: Sha256::new(),
    };
    let mut archive = tar::Builder::new(writer);
    archive.follow_symlinks(false);
    archive.sparse(false);
    let mut header = tar::Header::new_gnu();
    header.set_size(metadata.len() as u64);
    header.set_mode(0o600);
    archive.append_data(&mut header, "nanovm.json", &metadata[..])?;
    archive.append_dir("data", volume)?;
    let mut pending = vec![volume.to_path_buf()];
    let mut index = 0u64;
    let mut files = HashMap::new();
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if !kind.is_dir() && !kind.is_file() {
                return Err(invalid("unsupported capture file type"));
            }
            let path = entry.path();
            let relative = path.strip_prefix(volume).expect("inside volume");
            let md = entry.metadata()?;
            let inherits = kind.is_file() && capture.baseline.get(&path) == Some(&md.ino());
            let ranges: Vec<_> = if inherits {
                changed(md.ino(), md.len())
                    .into_iter()
                    .filter(|r| r.start < r.end)
                    .collect()
            } else {
                vec![]
            };
            let payload: u64 = ranges.iter().map(|r| r.end - r.start).sum();
            // Small or mostly changed files cost less than extra TAR records.
            if inherits && payload.saturating_add((ranges.len() as u64 + 1) * 1024) < md.len() {
                // The complete namespace tells a restorer which previous paths
                // still exist. Range payloads contain final bytes, never undo.
                let metadata = serde_json::to_vec(&Inherit { length: md.len() })?;
                let mut header = tar::Header::new_gnu();
                header.set_size(metadata.len() as u64);
                header.set_mode(0o600);
                archive.append_data(&mut header, Path::new("inherit").join(relative), &metadata[..])?;
                let mut file = File::open(&path)?;
                for range in ranges {
                    file.seek(SeekFrom::Start(range.start))?;
                    let mut header = tar::Header::new_gnu();
                    header.set_size(range.end - range.start);
                    header.set_mode(0o600);
                    archive.append_data(
                        &mut header,
                        format!("ranges/{index}/{}", range.start),
                        (&mut file).take(range.end - range.start),
                    )?;
                }
                index += 1;
            } else {
                archive.append_path_with_name(&path, Path::new("data").join(relative))?;
            }
            if kind.is_dir() {
                pending.push(path);
            } else {
                files.insert(path, md.ino());
            }
        }
    }
    let writer = archive.into_inner()?;
    sync(&staged)?;
    sync(journal)?;
    // Journal poisoning prevents reuse if publication later fails. Reopening
    // derives these caches from the recovered immutable history, never a cursor.
    capture.sequence = version.sequence;
    capture.used = limit - writer.remaining;
    capture.digest = format!("{:x}", writer.hash.finalize());
    capture.baseline = files;
    Ok(())
}

// Must precede rollback, including rollback's eventual deletion of the log:
// otherwise a second recovery could mistake a rejected version for a commit.
pub(super) fn discard(journal: &Path) -> io::Result<()> {
    match fs::remove_file(journal.join(STAGED)) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // An earlier removal may be visible but not durable after a failed sync.
    sync(journal)
}

// Called only after the log's deletion is durable, or on recovery without a log.
pub(super) fn finish(journal: &Path, out: &Path, obj: &ObjShared) -> io::Result<()> {
    let staged = journal.join(STAGED);
    let file = match File::open(&staged) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    // Recovery may observe a log unlink whose directory sync never completed.
    // Establish that decision before making any capture visible to a receiver.
    sync(journal)?;
    publish(file, journal, out, obj)
}

fn read_version(file: File, obj: &ObjShared) -> io::Result<Version> {
    let mut archive = tar::Archive::new(file);
    let mut entries = archive.entries()?;
    let mut entry = entries.next().ok_or_else(|| invalid("missing capture identity"))??;
    if entry.path()? != Path::new("nanovm.json") || entry.size() > 4096 {
        return Err(invalid("invalid capture identity"));
    }
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes)?;
    let version: Version = serde_json::from_slice(&bytes)?;
    if version.format != 2 || version.app != *obj.app || version.object != *obj.id || version.sequence == 0 {
        return Err(invalid("wrong capture identity"));
    }
    Ok(version)
}

fn publish(file: File, journal: &Path, out: &Path, obj: &ObjShared) -> io::Result<()> {
    let staged = journal.join(STAGED);
    let version = read_version(file, obj)?;
    let (last, _) = head(out)?;
    let target = out.join(filename(version.sequence));
    if version.sequence == last {
        if digest(&staged)? != digest(&target)? {
            return Err(invalid("conflicting committed version"));
        }
    } else {
        let parent = if last == 0 {
            String::new()
        } else {
            digest(&out.join(filename(last)))?
        };
        if last.checked_add(1) != Some(version.sequence) || version.parent != parent {
            return Err(invalid("out-of-order committed version"));
        }
        // Keep the durable staged name until the destination name is durable.
        // A rename could lose the only recoverable name between directory syncs.
        fs::hard_link(&staged, &target)?;
    }
    sync(out)?;
    // The output name is durable. A resurrected staging name is now an
    // idempotent publication retry, not a lost accepted version. The next
    // journal creation/staging barrier persists cleanup before another commit.
    fs::remove_file(&staged)
}
