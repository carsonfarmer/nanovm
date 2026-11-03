//! Storage-only comparison, not a replacement WASI filesystem.
use anyhow::{Result, bail, ensure};
use nanovm::{journal::Journal, object::ObjShared};
use redb::{Database, Durability, ReadableTable, ReadableTableMetadata, TableDefinition, WriteTransaction};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::{FileExt, MetadataExt},
        process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

const PAGE: usize = 4096;
const LENGTHS: TableDefinition<&str, u64> = TableDefinition::new("lengths");
const PAGES: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("pages");
const BLOBS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("blobs");
const REFS: TableDefinition<&[u8], u64> = TableDefinition::new("refs");
const KINDS: [&str; 3] = ["journal", "pages", "hashed"];

#[expect(
    clippy::large_enum_variant,
    reason = "One backend per case; no allocation just to shrink its stack size"
)]
enum Backend {
    Journal {
        root: PathBuf,
        journal: Journal,
    },
    Pages {
        db: Database,
        tx: Option<WriteTransaction>,
        hashed: bool,
        dirty: bool,
    },
}

impl Backend {
    fn audit(&self) -> Result<()> {
        if let Self::Pages {
            tx: Some(tx),
            hashed: true,
            ..
        } = self
        {
            let mut expected = BTreeMap::<Vec<u8>, u64>::new();
            for entry in tx.open_table(PAGES)?.iter()? {
                let (_, hash) = entry?;
                *expected.entry(hash.value().to_vec()).or_default() += 1;
            }
            let refs = tx.open_table(REFS)?;
            let blobs = tx.open_table(BLOBS)?;
            ensure!(
                refs.len()? == expected.len() as u64 && blobs.len()? == expected.len() as u64,
                "orphan content"
            );
            for (hash, count) in expected {
                ensure!(
                    refs.get(hash.as_slice())?.expect("reference").value() == count,
                    "reference count"
                );
                let bytes = blobs.get(hash.as_slice())?.expect("blob");
                ensure!(
                    Sha256::digest(bytes.value()).as_slice() == hash,
                    "content hash mismatch"
                );
            }
        }
        Ok(())
    }

    fn open(kind: &str, root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        if kind == "journal" {
            let root = root.canonicalize()?;
            let journal = Journal::open(&root, 1 << 30, Arc::new(ObjShared::new("bench".into(), "o")))?;
            Ok(Self::Journal { root, journal })
        } else {
            ensure!(matches!(kind, "pages" | "hashed"), "unknown backend {kind}");
            let db = Database::create(root.join("store.redb"))?;
            let mut tx = db.begin_write()?;
            tx.set_durability(Durability::Immediate)?;
            // Conservative untrusted-data policy, identical for both candidates.
            tx.set_two_phase_commit(true);
            tx.open_table(LENGTHS)?;
            tx.open_table(PAGES)?;
            tx.open_table(BLOBS)?;
            tx.open_table(REFS)?;
            tx.commit()?;
            Ok(Self::Pages {
                db,
                tx: None,
                hashed: kind == "hashed",
                dirty: false,
            })
        }
    }

    fn begin(&mut self) -> Result<()> {
        if let Self::Pages { db, tx, dirty, .. } = self {
            ensure!(tx.is_none(), "transaction already active");
            let mut next = db.begin_write()?;
            next.set_durability(Durability::Immediate)?;
            next.set_two_phase_commit(true);
            *tx = Some(next);
            *dirty = false;
        }
        Ok(())
    }

    fn read(&self, name: &str) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Journal { root, .. } => match fs::read(root.join(name)) {
                Ok(data) => Ok(Some(data)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            },
            Self::Pages { tx, hashed, .. } => {
                let tx = tx.as_ref().expect("active transaction");
                let lengths = tx.open_table(LENGTHS)?;
                let Some(len) = lengths.get(name)?.map(|v| v.value() as usize) else {
                    return Ok(None);
                };
                let mut result = vec![0; len];
                for (i, dst) in result.chunks_mut(PAGE).enumerate() {
                    let page = page(tx, *hashed, name, i as u64)?;
                    let n = dst.len().min(page.len());
                    dst[..n].copy_from_slice(&page[..n]);
                }
                Ok(Some(result))
            }
        }
    }

    fn write(&mut self, name: &str, offset: usize, data: &[u8]) -> Result<()> {
        match self {
            Self::Journal { root, journal } => {
                let path = root.join(name);
                let created = !path.exists();
                if created {
                    journal.creating(&path)?;
                }
                let file = Arc::new(
                    File::options()
                        .create(true)
                        .truncate(false)
                        .read(true)
                        .write(true)
                        .open(&path)?,
                );
                if created {
                    journal.created(&path)?;
                }
                journal.opened(&file)?;
                journal.writing(&file, offset as u64, (offset + data.len()) as u64)?;
                file.write_all_at(data, offset as u64)?;
            }
            Self::Pages { tx, hashed, dirty, .. } => {
                let tx = tx.as_ref().expect("active transaction");
                let mut lengths = tx.open_table(LENGTHS)?;
                let old_len = lengths.get(name)?.map_or(0, |v| v.value());
                lengths.insert(name, old_len.max((offset + data.len()) as u64))?;
                let mut at = offset;
                let mut rest = data;
                while !rest.is_empty() {
                    let index = (at / PAGE) as u64;
                    let within = at % PAGE;
                    let n = rest.len().min(PAGE - within);
                    let mut bytes = page(tx, *hashed, name, index)?;
                    bytes.resize(bytes.len().max(within + n), 0);
                    bytes[within..within + n].copy_from_slice(&rest[..n]);
                    put_page(tx, *hashed, name, index, &bytes)?;
                    at += n;
                    rest = &rest[n..];
                }
                *dirty = true;
            }
        }
        Ok(())
    }

    fn delete(&mut self, name: &str) -> Result<()> {
        match self {
            Self::Journal { root, journal } => {
                let path = root.join(name);
                if path.exists() {
                    journal.unlink(&path, path.metadata()?.len())?;
                }
            }
            Self::Pages { tx, hashed, dirty, .. } => {
                let tx = tx.as_ref().expect("active transaction");
                let len = tx.open_table(LENGTHS)?.remove(name)?.map_or(0, |v| v.value());
                for i in 0..len.div_ceil(PAGE as u64) {
                    remove_page(tx, *hashed, name, i)?;
                }
                *dirty = true;
            }
        }
        Ok(())
    }

    fn replace(&mut self, name: &str, data: &[u8]) -> Result<()> {
        if matches!(self, Self::Journal { .. }) {
            self.write(".stage", 0, data)?;
            let Self::Journal { root, journal } = self else {
                unreachable!()
            };
            let from = root.join(".stage");
            let to = root.join(name);
            let held = if to.exists() {
                let bytes = to.metadata()?.len();
                Some((journal.remove(&to, bytes)?, bytes))
            } else {
                None
            };
            journal.renaming(&from, &to)?;
            fs::rename(&from, &to)?;
            journal.renamed(&from, &to);
            if let Some((slot, bytes)) = held {
                journal.discard_created(&slot, bytes)?;
            }
        } else {
            // Same-index updates let the hashed variant detect unchanged pages.
            // Remove only the old tail; write replaces all bytes in the new range.
            let Self::Pages { tx, hashed, .. } = self else {
                unreachable!()
            };
            let tx = tx.as_ref().expect("active transaction");
            let old = tx.open_table(LENGTHS)?.get(name)?.map_or(0, |v| v.value());
            for i in data.len().div_ceil(PAGE) as u64..old.div_ceil(PAGE as u64) {
                remove_page(tx, *hashed, name, i)?;
            }
            // Last page must lose its old tail on shrink.
            for (i, bytes) in data.chunks(PAGE).enumerate() {
                put_page(tx, *hashed, name, i as u64, bytes)?;
            }
            tx.open_table(LENGTHS)?.insert(name, data.len() as u64)?;
            let Self::Pages { dirty, .. } = self else {
                unreachable!()
            };
            *dirty = true;
        }
        Ok(())
    }

    fn end(&mut self, commit: bool) -> Result<()> {
        match self {
            Self::Journal { root, journal } => {
                if commit {
                    journal.commit()?;
                } else {
                    // Recovery runs when the rejected request's instance restarts.
                    *journal = Journal::open(root, 1 << 30, Arc::new(ObjShared::new("bench".into(), "o")))?;
                }
            }
            Self::Pages { tx, dirty, .. } => {
                let tx = tx.take().expect("active transaction");
                if commit && *dirty {
                    tx.commit()?;
                } else {
                    tx.abort()?;
                }
            }
        }
        Ok(())
    }
}

fn page(tx: &WriteTransaction, hashed: bool, name: &str, index: u64) -> Result<Vec<u8>> {
    let pages = tx.open_table(PAGES)?;
    let Some(v) = pages.get((name, index))? else {
        return Ok(Vec::new());
    };
    if hashed {
        Ok(tx
            .open_table(BLOBS)?
            .get(v.value())?
            .expect("referenced blob")
            .value()
            .to_vec())
    } else {
        Ok(v.value().to_vec())
    }
}

fn remove_page(tx: &WriteTransaction, hashed: bool, name: &str, index: u64) -> Result<()> {
    let old = tx.open_table(PAGES)?.remove((name, index))?.map(|v| v.value().to_vec());
    if hashed && let Some(hash) = old {
        let mut refs = tx.open_table(REFS)?;
        let n = refs.get(hash.as_slice())?.expect("reference count").value();
        if n == 1 {
            refs.remove(hash.as_slice())?;
            tx.open_table(BLOBS)?.remove(hash.as_slice())?;
        } else {
            refs.insert(hash.as_slice(), n - 1)?;
        }
    }
    Ok(())
}

fn put_page(tx: &WriteTransaction, hashed: bool, name: &str, index: u64, bytes: &[u8]) -> Result<()> {
    if !hashed {
        let mut pages = tx.open_table(PAGES)?;
        if !pages.get((name, index))?.is_some_and(|v| v.value() == bytes) {
            pages.insert((name, index), bytes)?;
        }
        return Ok(());
    }
    let hash = Sha256::digest(bytes);
    {
        let pages = tx.open_table(PAGES)?;
        if pages.get((name, index))?.is_some_and(|v| v.value() == hash.as_slice()) {
            return Ok(());
        }
    }
    remove_page(tx, true, name, index)?;
    let mut refs = tx.open_table(REFS)?;
    let n = refs.get(hash.as_slice())?.map_or(0, |v| v.value());
    if n == 0 {
        tx.open_table(BLOBS)?.insert(hash.as_slice(), bytes)?;
    }
    refs.insert(hash.as_slice(), n + 1)?;
    tx.open_table(PAGES)?.insert((name, index), hash.as_slice())?;
    Ok(())
}

// Deterministic incompressible-ish payloads; distinct generations do not dedup.
fn payload(len: usize, generation: u64) -> Vec<u8> {
    let mut state = generation + 0x1234_5678;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

fn storage(path: &Path) -> Result<(u64, u64)> {
    let mut bytes = 0;
    let mut files = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let md = entry.metadata()?;
        if md.is_dir() {
            let (b, f) = storage(&entry.path())?;
            bytes += b;
            files += f;
        } else {
            bytes += md.blocks() * 512;
            files += 1;
        }
    }
    Ok((bytes, files))
}

fn verify(kind: &str, root: &Path) -> Result<()> {
    let mut b = Backend::open(kind, root)?;
    let original = payload(9001, 1);
    b.begin()?;
    b.replace("a", &original)?;
    b.replace("b", &original)?;
    b.end(true)?;
    b.begin()?;
    b.write("a", 4093, &[1, 2, 3, 4, 5, 6, 7])?;
    let mut expected = original.clone();
    expected[4093..4100].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7]);
    ensure!(b.read("a")? == Some(expected), "read-your-writes");
    b.delete("b")?;
    ensure!(b.read("b")?.is_none());
    b.end(false)?;
    drop(b);
    let mut b = Backend::open(kind, root)?;
    b.begin()?;
    ensure!(b.read("a")? == Some(original.clone()));
    ensure!(b.read("b")? == Some(original));
    b.replace("a", b"short")?;
    b.write("a", 8195, b"end")?;
    let mut expected = vec![0; 8198];
    expected[..5].copy_from_slice(b"short");
    expected[8195..].copy_from_slice(b"end");
    ensure!(b.read("a")? == Some(expected.clone()), "shrink/sparse extension");
    b.delete("b")?;
    b.replace("empty", b"")?;
    b.end(true)?;
    drop(b);
    let mut b = Backend::open(kind, root)?;
    b.begin()?;
    ensure!(b.read("a")? == Some(expected));
    ensure!(b.read("b")?.is_none());
    ensure!(b.read("empty")? == Some(vec![]));
    b.audit()?;
    b.end(true)?;
    Ok(())
}

fn crash_child(kind: &str, root: &Path, stage: &str) -> Result<()> {
    let mut b = Backend::open(kind, root)?;
    b.begin()?;
    b.replace("a", b"new-a")?;
    b.replace("b", b"new-b")?;
    if stage == "after" {
        b.end(true)?;
    }
    println!("ready");
    std::io::stdout().flush()?;
    if stage == "during" {
        b.end(true)?;
    }
    loop {
        std::thread::park();
    }
}

fn crash_checks(kind: &str, root: &Path) -> Result<()> {
    for stage in ["before", "during", "after"] {
        let mut b = Backend::open(kind, root)?;
        b.begin()?;
        b.replace("a", b"old-a")?;
        b.replace("b", b"old-b")?;
        b.end(true)?;
        drop(b);
        let mut child = Command::new(std::env::current_exe()?)
            .args(["child", kind])
            .arg(root)
            .arg(stage)
            .stdout(Stdio::piped())
            .spawn()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("piped stdout")).read_line(&mut line)?;
        ensure!(line.trim() == "ready", "child did not reach cut");
        child.kill()?;
        ensure!(child.wait()?.signal() == Some(9), "not SIGKILL");
        let mut b = Backend::open(kind, root)?;
        b.begin()?;
        let a = b.read("a")?.expect("a");
        let c = b.read("b")?.expect("b");
        let old = a == b"old-a" && c == b"old-b";
        let new = a == b"new-a" && c == b"new-b";
        ensure!(
            match stage {
                "before" => old,
                "after" => new,
                _ => old || new,
            },
            "torn {kind} {stage}"
        );
        b.audit()?;
        b.end(true)?;
    }
    Ok(())
}

fn written_bytes() -> Result<u64> {
    let io = fs::read_to_string("/proc/self/io")?;
    Ok(io
        .lines()
        .find_map(|line| line.strip_prefix("write_bytes: "))
        .expect("Linux I/O accounting")
        .trim()
        .parse()?)
}

fn bench(kind: &str, root: &Path, workload: &str, size: usize, count: usize, round: usize) -> Result<()> {
    let mut b = Backend::open(kind, root)?;
    let initial = payload(size, 0);
    b.begin()?;
    b.replace("value", &initial)?;
    b.end(true)?;
    let input_size = match workload {
        "read" | "identical" => 0,
        "page-write" => PAGE,
        "batch" => size * 32,
        _ => size,
    };
    let inputs: Vec<_> = (0..count).map(|i| payload(input_size, i as u64 + 1)).collect();
    let mut expected = initial.clone();
    let mut timings = Vec::new();
    let mut end_ns = 0u128;
    let writes_before = written_bytes()?;
    for (i, data) in inputs.iter().enumerate() {
        let start = Instant::now();
        b.begin()?;
        match workload {
            "read" => {
                ensure!(b.read("value")?.as_ref() == Some(&expected), "read mismatch");
            }
            "replace" | "reject" => {
                b.replace("value", data)?;
            }
            "identical" => b.replace("value", &initial)?,
            "page-write" => b.write("value", ((i * 17) % (size / PAGE)) * PAGE, data)?,
            "many-files" => b.replace(&format!("f{i}"), data)?,
            "batch" => {
                for j in 0..32 {
                    b.replace(&format!("f{j}"), &data[j * size..(j + 1) * size])?;
                }
            }
            "shared-batch" => {
                for j in 0..32 {
                    b.replace(&format!("f{j}"), data)?;
                }
            }
            _ => bail!("unknown workload"),
        }
        let end = Instant::now();
        b.end(workload != "reject")?;
        end_ns += end.elapsed().as_nanos();
        timings.push(start.elapsed());
        match workload {
            "replace" => expected.clone_from(data),
            "page-write" => {
                let at = ((i * 17) % (size / PAGE)) * PAGE;
                expected[at..at + PAGE].copy_from_slice(data);
            }
            _ => {}
        }
    }
    let write_bytes = written_bytes()? - writes_before;
    drop(b);
    let mut b = Backend::open(kind, root)?;
    b.begin()?;
    ensure!(b.read("value")? == Some(expected), "persisted value mismatch");
    if workload == "many-files" {
        for (i, data) in inputs.iter().enumerate() {
            ensure!(b.read(&format!("f{i}"))?.as_ref() == Some(data));
        }
    }
    if workload == "batch" {
        for j in 0..32 {
            ensure!(b.read(&format!("f{j}"))?.as_deref() == Some(&inputs[count - 1][j * size..(j + 1) * size]));
        }
    }
    if workload == "shared-batch" {
        for j in 0..32 {
            ensure!(b.read(&format!("f{j}"))?.as_ref() == inputs.last());
        }
    }
    b.audit()?;
    b.end(true)?;
    drop(b);
    let (allocated, files) = storage(root)?;
    let total: Duration = timings.iter().sum();
    timings.sort();
    println!(
        "{}",
        serde_json::json!({"backend":kind,"workload":workload,"size":size,"count":count,"round":round,
        "mean_us":total.as_secs_f64()*1e6/count as f64,"p50_us":timings[count/2].as_secs_f64()*1e6,
        "p99_us":timings[(count*99/100).min(count-1)].as_secs_f64()*1e6,"end_us":end_ns as f64/count as f64/1000.0,
        "allocated_bytes":allocated,"files":files,"write_bytes":write_bytes})
    );
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("child") {
        return crash_child(&args[2], Path::new(&args[3]), &args[4]);
    }
    let root = std::env::current_dir()?.join(format!("target/fs-backends-{}", std::process::id()));
    ensure!(!root.exists(), "refuse existing run directory");
    fs::create_dir_all(&root)?;
    for kind in KINDS {
        verify(kind, &root.join(format!("verify-{kind}")))?;
        crash_checks(kind, &root.join(format!("crash-{kind}")))?;
    }
    eprintln!(
        "PASS visibility, rejection, shrink/sparse writes, deletion, hash reference audit, reopen and SIGKILL before/racing/after commit"
    );
    if args.get(1).map(String::as_str) == Some("check") {
        fs::remove_dir_all(root)?;
        return Ok(());
    }
    let cases = [
        ("read", 128, 500),
        ("read", 16384, 500),
        ("read", 1048576, 100),
        ("replace", 128, 100),
        ("replace", 16384, 100),
        ("replace", 1048576, 40),
        ("identical", 1048576, 40),
        ("page-write", 1048576, 100),
        ("reject", 16384, 100),
        ("many-files", 128, 1000),
        ("batch", 128, 40),
        ("shared-batch", 16384, 40),
    ];
    for round in 0..3 {
        for (workload, size, count) in cases {
            for i in 0..3 {
                let kind = KINDS[(i + round) % 3];
                bench(
                    kind,
                    &root.join(format!("{round}-{workload}-{size}-{kind}")),
                    workload,
                    size,
                    count,
                    round,
                )?;
            }
        }
    }
    fs::remove_dir_all(root)?;
    Ok(())
}
