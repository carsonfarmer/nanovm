//! W-9 storage-only protocol experiment. Not a runtime replication API.
//! Run with `cargo run --locked -p nanovm --example committed_state`.
//! The fixture owns the writer exclusively while copying after Journal::commit.
//! Production capture/commit atomicity, authentication and fencing are NOT supplied.

use anyhow::{Result, ensure};
use nanovm::{journal::Journal, object::ObjShared};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

// Deliberately tiny experiment limits, not proposed deployment settings.
const MAX_BYTES: usize = 1 << 20;
const MAX_ENTRIES: usize = 256;
const MAX_VERSIONS: u64 = 16;
type Tree = BTreeMap<String, Option<Vec<u8>>>;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Version {
    app: String,
    object: String,
    epoch: u64,
    sequence: u64,
    parent: String,
    tree: Tree,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_BYTES, "version exceeds byte limit");
    Ok(bytes)
}

fn validate(v: &Version) -> Result<()> {
    // Fixed stream identity is supplied out of band by this experiment.
    // An epoch comparison is not a lease or a storage-enforced fence.
    ensure!(v.app == "app" && v.object == "o" && v.epoch == 7, "wrong stream");
    ensure!((1..=MAX_VERSIONS).contains(&v.sequence), "sequence limit");
    ensure!(v.tree.len() <= MAX_ENTRIES, "entry limit");
    for name in v.tree.keys() {
        let path = Path::new(name);
        ensure!(
            name.split('/')
                .all(|p| !p.is_empty() && p != "." && p != ".." && !p.contains('\0')),
            "unsafe path"
        );
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            ensure!(v.tree.get(parent.to_str().unwrap()) == Some(&None), "missing directory");
        }
    }
    Ok(())
}

fn tree(root: &Path) -> Result<Tree> {
    let mut out = Tree::new();
    let mut pending = vec![root.to_path_buf()];
    let mut bytes = 0;
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            ensure!(out.len() < MAX_ENTRIES, "entry limit");
            let path = entry.path();
            let name = path
                .strip_prefix(root)?
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF8 path"))?
                .to_owned();
            let kind = entry.file_type()?;
            let value = if kind.is_dir() {
                pending.push(path);
                None
            } else {
                ensure!(kind.is_file(), "unsupported file type");
                let content = read(&path)?;
                bytes += content.len();
                ensure!(bytes <= MAX_BYTES, "tree byte limit");
                Some(content)
            };
            out.insert(name, value);
        }
    }
    Ok(out)
}

// Must run under exclusive mutation ownership, after a successful local commit
// and before any subsequent guest execution (including post-return execution).
fn capture(volume: &Path, sequence: u64, parent: String) -> Result<Vec<u8>> {
    ensure!(
        !volume.parent().unwrap().join("~journal/o/log").try_exists()?,
        "tentative state"
    );
    let version = Version {
        app: "app".into(),
        object: "o".into(),
        epoch: 7,
        sequence,
        parent,
        tree: tree(volume)?,
    };
    validate(&version)?;
    let wire = serde_json::to_vec(&version)?;
    ensure!(wire.len() <= MAX_BYTES, "encoded byte limit");
    Ok(wire)
}

// A receiver owns an existing private root. Immutable files ARE its cursor:
// no separately updated pointer can acknowledge an absent version after a crash.
fn receive(root: &Path, wire: &[u8], digest: &str) -> Result<()> {
    ensure!(wire.len() <= MAX_BYTES && hash(wire) == digest, "corrupt transfer");
    let version: Version = serde_json::from_slice(wire)?;
    validate(&version)?;
    let lock = File::open(root)?;
    lock.try_lock()?;
    let mut parent = String::new();
    let mut next = 1;
    while next <= MAX_VERSIONS && root.join(next.to_string()).try_exists()? {
        let previous = read(&root.join(next.to_string()))?;
        let v: Version = serde_json::from_slice(&previous)?;
        validate(&v)?;
        ensure!(v.sequence == next && v.parent == parent, "broken receiver chain");
        parent = hash(&previous);
        next += 1;
    }
    let destination = root.join(version.sequence.to_string());
    if version.sequence < next {
        ensure!(read(&destination)? == wire, "conflicting retry");
        // Retry also completes a possibly interrupted publication-directory sync.
        lock.sync_all()?;
        return Ok(());
    }
    ensure!(
        version.sequence == next && version.parent == parent,
        "gap or wrong parent"
    );
    let staging = root.join("incoming");
    if staging.try_exists()? {
        fs::remove_file(&staging)?;
    }
    let mut file = File::options().write(true).create_new(true).open(&staging)?;
    file.write_all(wire)?;
    cut("written");
    file.sync_all()?;
    cut("synced");
    // Unlike rename, this refuses to replace an existing immutable version.
    fs::hard_link(&staging, destination)?;
    cut("linked");
    lock.sync_all()?;
    cut("published");
    fs::remove_file(staging)?;
    Ok(())
}

// Abrupt receiver exit without destructors; exercised by the executable drill.
fn cut(stage: &str) {
    if std::env::var("NANOVM_W9_CUT").as_deref() == Ok(stage) {
        std::process::exit(77);
    }
}

// Restore only an explicitly identified, verified receipt into a fresh root.
// A failed root is never served. Promotion/routing is outside this experiment.
fn restore(wire: &[u8], digest: &str, root: &Path) -> Result<()> {
    ensure!(
        wire.len() <= MAX_BYTES && hash(wire) == digest,
        "corrupt recovery point"
    );
    let version: Version = serde_json::from_slice(wire)?;
    validate(&version)?;
    fs::create_dir(root)?;
    for (name, value) in version.tree {
        let path = root.join(name);
        if let Some(bytes) = value {
            let mut file = File::options().write(true).create_new(true).open(path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        } else {
            fs::create_dir(path)?;
        }
    }
    // Sync directories bottom-up, including the fresh root's name.
    let mut dirs = vec![root.to_path_buf()];
    for (name, value) in tree(root)? {
        if value.is_none() {
            dirs.push(root.join(name));
        }
    }
    dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for dir in dirs {
        File::open(dir)?.sync_all()?;
    }
    File::open(root.parent().unwrap())?.sync_all()?;
    Ok(())
}

fn journal(root: &Path) -> Result<Journal> {
    Ok(Journal::open(
        root,
        MAX_BYTES as u64,
        Arc::new(ObjShared::new("app".into(), "o")),
    )?)
}

fn write(j: &mut Journal, root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let path = root.join(name);
    j.creating(&path)?;
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    j.writing(&file, 0, bytes.len() as u64)?;
    file.write_all_at(bytes, 0)?;
    Ok(())
}

fn drill(transfer: impl Fn(&Path, &[u8]) -> Result<()>) -> Result<()> {
    let root = std::env::temp_dir().join(format!(
        "nanovm-w9-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    fs::create_dir(&root)?;
    let _cleanup = scopeguard::guard(root.clone(), |p| {
        let _ = fs::remove_dir_all(p);
    });
    let source = root.join("source");
    let volume = source.join("o");
    let receiver = root.join("receiver");
    fs::create_dir_all(&volume)?;
    fs::create_dir(&receiver)?;
    let mut j = journal(&volume)?;
    write(&mut j, &volume, "data", b"response")?;
    let empty = volume.join("empty");
    j.creating(&empty)?;
    fs::create_dir(&empty)?;
    ensure!(capture(&volume, 1, String::new()).is_err(), "captured tentative bytes");
    j.commit()?;
    let first = capture(&volume, 1, String::new())?;
    // Next commit models post-return changes; it is NOT the same recovery point.
    write(&mut j, &volume, "data", b"post-ret")?;
    j.renaming(&volume.join("data"), &volume.join("renamed"))?;
    fs::rename(volume.join("data"), volume.join("renamed"))?;
    j.renamed(&volume.join("data"), &volume.join("renamed"));
    j.removing_dir(&empty)?;
    fs::remove_dir(&empty)?;
    j.commit()?;
    let second = capture(&volume, 2, hash(&first))?;
    ensure!(
        serde_json::from_slice::<Version>(&first)?.tree["data"] == Some(b"response".to_vec()),
        "mutable capture"
    );
    ensure!(receive(&receiver, &second, &hash(&second)).is_err(), "accepted gap");
    let mut corrupted: Version = serde_json::from_slice(&first)?;
    corrupted.tree.insert("data".into(), Some(b"tampered".to_vec()));
    ensure!(
        receive(&receiver, &serde_json::to_vec(&corrupted)?, &hash(&first)).is_err(),
        "ignored digest"
    );
    transfer(&receiver, &first)?;
    transfer(&receiver, &first)?;
    transfer(&receiver, &second)?;
    // A partial next transfer is not an accepted version or a new cursor.
    fs::write(receiver.join("incoming"), &second[..second.len() / 2])?;
    transfer(&receiver, &second)?;
    let mut bad: Version = serde_json::from_slice(&second)?;
    bad.tree.insert("renamed".into(), Some(b"conflict".to_vec()));
    let conflict = serde_json::to_vec(&bad)?;
    ensure!(
        receive(&receiver, &conflict, &hash(&conflict)).is_err(),
        "replaced immutable receipt"
    );
    ensure!(
        receive(&receiver, &second[..second.len() - 1], &hash(&second)).is_err(),
        "accepted corruption"
    );
    bad.sequence = 3;
    let wrong_parent = serde_json::to_vec(&bad)?;
    ensure!(
        receive(&receiver, &wrong_parent, &hash(&wrong_parent)).is_err(),
        "accepted wrong parent"
    );
    bad.parent = hash(&second);
    bad.epoch += 1;
    let wrong_epoch = serde_json::to_vec(&bad)?;
    ensure!(
        receive(&receiver, &wrong_epoch, &hash(&wrong_epoch)).is_err(),
        "accepted wrong epoch"
    );
    write(&mut j, &volume, "renamed", b"rejected")?;
    drop(j);
    let mut j = journal(&volume)?;
    ensure!(fs::read(volume.join("renamed"))? == b"post-ret", "rejection leaked");
    // Deliberate counterexample: local acceptance alone does not mean receipt.
    write(&mut j, &volume, "renamed", b"unsent!!")?;
    j.commit()?;
    drop(j);
    fs::remove_dir_all(&source)?;
    let receipt = read(&receiver.join("2"))?;
    let initial = root.join("initial");
    restore(&read(&receiver.join("1"))?, &hash(&first), &initial)?;
    ensure!(
        tree(&initial)? == BTreeMap::from([("data".into(), Some(b"response".to_vec())), ("empty".into(), None)]),
        "first recovery point changed"
    );
    let recovered = root.join("recovered");
    restore(&receipt, &hash(&second), &recovered)?;
    ensure!(
        tree(&recovered)? == BTreeMap::from([("renamed".into(), Some(b"post-ret".to_vec()))]),
        "wrong recovered namespace/bytes"
    );
    ensure!(
        restore(&receipt, &hash(&second), &recovered).is_err(),
        "overwrote recovery root"
    );
    let mut j = journal(&recovered)?;
    write(&mut j, &recovered, "renamed", b"new-life")?;
    j.commit()?;
    drop(j);
    drop(journal(&recovered)?);
    ensure!(
        fs::read(recovered.join("renamed"))? == b"new-life",
        "recovered root cannot commit"
    );
    println!("W-9: immutable versions, ordered/idempotent receipt, rejection and source-loss recovery passed");
    println!("Local-only accepted third write was NOT recovered: no remote acknowledgement/RPO claim");
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() == 5 && args[1] == "receive" {
        return receive(
            Path::new(&args[2]),
            &read(Path::new(&args[3]))?,
            args[4].to_str().unwrap(),
        );
    }
    ensure!(args.len() == 1, "usage: committed_state [receive ROOT FILE SHA256]");
    drill(|root, wire| {
        let transport = root.parent().unwrap().join("transport");
        fs::write(&transport, wire)?;
        if serde_json::from_slice::<Version>(wire)?.sequence == 1 {
            for stage in ["written", "synced", "linked", "published"] {
                let cut_root = root.parent().unwrap().join(format!("cut-{stage}"));
                if cut_root.exists() {
                    continue;
                }
                fs::create_dir(&cut_root)?;
                let status = Command::new(std::env::current_exe()?)
                    .arg("receive")
                    .arg(&cut_root)
                    .arg(&transport)
                    .arg(hash(wire))
                    .env("NANOVM_W9_CUT", stage)
                    .status()?;
                ensure!(status.code() == Some(77), "cut did not execute: {stage}");
                ensure!(
                    cut_root.join("1").exists() == matches!(stage, "linked" | "published"),
                    "unexpected publication at {stage}"
                );
                receive(&cut_root, wire, &hash(wire))?;
                ensure!(read(&cut_root.join("1"))? == wire, "cut recovery changed bytes");
            }
        }
        ensure!(
            Command::new(std::env::current_exe()?)
                .arg("receive")
                .arg(root)
                .arg(transport)
                .arg(hash(wire))
                .status()?
                .success(),
            "receiver process failed"
        );
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_versions_survive_source_loss() -> Result<()> {
        drill(|root, wire| receive(root, wire, &hash(wire)))
    }

    #[test]
    fn invalid_versions_are_refused() -> Result<()> {
        let base = Version {
            app: "app".into(),
            object: "o".into(),
            epoch: 7,
            sequence: 1,
            parent: String::new(),
            tree: Tree::new(),
        };
        for name in [
            "",
            "/escape",
            "../escape",
            "a/../escape",
            "a//b",
            "a/./b",
            "a/",
            "a\0b",
            "missing/child",
        ] {
            let mut v = base.clone();
            v.tree.insert(name.into(), Some(vec![]));
            ensure!(validate(&v).is_err(), "accepted {name:?}");
        }
        let mut v = base.clone();
        v.tree = (0..=MAX_ENTRIES).map(|i| (i.to_string(), None)).collect();
        ensure!(validate(&v).is_err(), "ignored entry limit");
        for sequence in [0, MAX_VERSIONS + 1] {
            let mut v = base.clone();
            v.sequence = sequence;
            ensure!(validate(&v).is_err(), "ignored sequence limit");
        }
        let mut v = base.clone();
        v.app = "other".into();
        ensure!(validate(&v).is_err(), "ignored app identity");
        v = base;
        v.object = "other".into();
        ensure!(validate(&v).is_err(), "ignored object identity");
        let oversized = vec![b' '; MAX_BYTES + 1];
        ensure!(
            receive(Path::new("/nonexistent"), &oversized, &hash(&oversized))
                .unwrap_err()
                .to_string()
                == "corrupt transfer",
            "ignored wire limit"
        );
        Ok(())
    }
}
