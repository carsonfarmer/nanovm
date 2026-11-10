//! Immutable full-volume captures, staged under the journal's commit decision.
//! The app's existing storage lock owns this tree; no distributed fence is implied.

use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{config::CaptureConfig, journal::sync, object::ObjShared};

const STAGED: &str = "capture";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Version {
    format: u32,
    app: String,
    object: String,
    sequence: u64,
    parent: String,
    /// The epoch of the node's lease it was committed under (W-10).
    epoch: u64,
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

pub(super) fn prepare(
    volume: &Path,
    journal: &Path,
    out: &Path,
    obj: &ObjShared,
    cap: &CaptureConfig,
) -> io::Result<()> {
    let (last, used) = head(out)?;
    let limit = cap
        .max_mb
        .checked_mul(1 << 20)
        .ok_or_else(|| invalid("capture limit overflow"))?;
    if last >= cap.max_versions || used >= limit {
        return Err(ErrorKind::StorageFull.into());
    }
    let version = Version {
        format: 2,
        app: obj.app.to_string(),
        object: obj.id.to_string(),
        sequence: last
            .checked_add(1)
            .ok_or_else(|| invalid("capture sequence overflow"))?,
        parent: if last == 0 {
            String::new()
        } else {
            digest(&out.join(filename(last)))?
        },
        epoch: obj.lease_epoch()?,
    };
    let metadata = serde_json::to_vec(&version)?;
    if metadata.len() > 4096 {
        return Err(invalid("capture identity too large"));
    }
    fs::create_dir_all(out)?;
    // Persist the archive directory's name before a commit can depend on it.
    for dir in [
        out,
        out.parent().expect("inside ~committed"),
        volume.parent().expect("inside app"),
    ] {
        sync(dir)?;
    }
    let staged = journal.join(STAGED);
    let writer = Bounded {
        file: File::options().write(true).create_new(true).open(&staged)?,
        remaining: limit - used,
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
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if !kind.is_dir() && !kind.is_file() {
                return Err(invalid("unsupported capture file type"));
            }
            let path = entry.path();
            archive.append_path_with_name(
                &path,
                Path::new("data").join(path.strip_prefix(volume).expect("inside volume")),
            )?;
            if kind.is_dir() {
                pending.push(path);
            }
        }
    }
    drop(archive.into_inner()?);
    sync(&staged)?;
    sync(journal)
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
    fs::remove_file(&staged)?;
    sync(journal)
}
