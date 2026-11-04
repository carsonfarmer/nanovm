//! Bounded flat-namespace inode proof; not a WASI adapter.
use super::{Backend, LENGTHS, PAGE, page, payload, put_page, remove_page, storage};
use anyhow::{Result, ensure};
use redb::{ReadableTable, TableDefinition, WriteTransaction};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::process::ExitStatusExt,
    path::Path,
    process::{Command, Stdio},
    time::Instant,
};

const NAMES: TableDefinition<&str, u64> = TableDefinition::new("names");

#[derive(Clone, Copy)]
struct Limits {
    bytes: u64,
    files: u64,
    handles: usize,
    cache: usize,
}

struct Volume {
    backend: Backend,
    handles: BTreeMap<u64, u64>,
    next_handle: u64,
    next_inode: u64,
    bytes: u64,
    files: u64,
    limits: Limits,
}

impl Volume {
    fn open(kind: &str, root: &Path, limits: Limits) -> Result<Self> {
        ensure!(matches!(kind, "pages" | "hashed"));
        let mut backend = Backend::open_cached(kind, root, limits.cache)?;
        backend.begin()?;
        let mut volume = Self {
            backend,
            handles: BTreeMap::new(),
            next_handle: 1,
            next_inode: 1,
            bytes: 0,
            files: 0,
            limits,
        };
        volume.tx().open_table(NAMES)?;
        volume.recover()?;
        Ok(volume)
    }

    fn tx(&self) -> &WriteTransaction {
        let Backend::Pages { tx: Some(tx), .. } = &self.backend else {
            unreachable!()
        };
        tx
    }

    fn dirty(&mut self) {
        let Backend::Pages { dirty, .. } = &mut self.backend else {
            unreachable!()
        };
        *dirty = true;
    }

    fn named(&self, name: &str) -> Result<Option<u64>> {
        Ok(self.tx().open_table(NAMES)?.get(name)?.map(|v| v.value()))
    }

    fn inode(&self, handle: u64) -> Result<u64> {
        self.handles
            .get(&handle)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("bad descriptor"))
    }

    fn len(&self, inode: u64) -> Result<u64> {
        Ok(self
            .tx()
            .open_table(LENGTHS)?
            .get(inode.to_string().as_str())?
            .expect("live inode")
            .value())
    }

    fn open_file(&mut self, name: &str) -> Result<u64> {
        ensure!(self.handles.len() < self.limits.handles, "descriptor quota");
        let inode = self.named(name)?.ok_or_else(|| anyhow::anyhow!("not found"))?;
        let handle = self.next_handle;
        self.next_handle += 1;
        self.handles.insert(handle, inode);
        Ok(handle)
    }

    fn create(&mut self, name: &str) -> Result<u64> {
        ensure!(
            !name.is_empty() && name.len() <= 255 && !name.contains('/') && name != "." && name != "..",
            "invalid name"
        );
        ensure!(self.named(name)?.is_none(), "exists");
        ensure!(self.files < self.limits.files, "file quota");
        ensure!(self.handles.len() < self.limits.handles, "descriptor quota");
        let inode = self.next_inode;
        self.next_inode += 1;
        self.backend.replace(&inode.to_string(), &[])?;
        self.tx().open_table(NAMES)?.insert(name, inode)?;
        self.files += 1;
        self.open_file(name)
    }

    fn read(&self, handle: u64) -> Result<Vec<u8>> {
        Ok(self
            .backend
            .read(&self.inode(handle)?.to_string())?
            .expect("live inode"))
    }

    fn write(&mut self, handle: u64, offset: usize, bytes: &[u8]) -> Result<()> {
        let inode = self.inode(handle)?;
        if bytes.is_empty() {
            return Ok(());
        }
        let old = self.len(inode)?;
        let end = offset
            .checked_add(bytes.len())
            .ok_or_else(|| anyhow::anyhow!("offset overflow"))? as u64;
        let growth = end.saturating_sub(old);
        ensure!(growth <= self.limits.bytes.saturating_sub(self.bytes), "byte quota");
        self.backend.write(&inode.to_string(), offset, bytes)?;
        self.bytes += growth;
        Ok(())
    }

    fn resize(&mut self, handle: u64, length: u64) -> Result<()> {
        let inode = self.inode(handle)?;
        let old = self.len(inode)?;
        ensure!(
            length.saturating_sub(old) <= self.limits.bytes.saturating_sub(self.bytes),
            "byte quota"
        );
        let Backend::Pages { hashed, .. } = self.backend else {
            unreachable!()
        };
        let name = inode.to_string();
        if length < old {
            for index in length.div_ceil(PAGE as u64)..old.div_ceil(PAGE as u64) {
                remove_page(self.tx(), hashed, &name, index)?;
            }
            if !length.is_multiple_of(PAGE as u64) {
                let index = length / PAGE as u64;
                let mut bytes = page(self.tx(), hashed, &name, index)?;
                bytes.truncate((length % PAGE as u64) as usize);
                put_page(self.tx(), hashed, &name, index, &bytes)?;
            }
        }
        self.tx().open_table(LENGTHS)?.insert(name.as_str(), length)?;
        self.bytes = self.bytes - old + length;
        self.dirty();
        Ok(())
    }

    fn collect(&mut self, inode: u64) -> Result<()> {
        if self.handles.values().any(|&id| id == inode) {
            return Ok(());
        }
        for entry in self.tx().open_table(NAMES)?.iter()? {
            if entry?.1.value() == inode {
                return Ok(());
            }
        }
        let length = self.len(inode)?;
        self.backend.delete(&inode.to_string())?;
        self.bytes -= length;
        self.files -= 1;
        Ok(())
    }

    fn close(&mut self, handle: u64) -> Result<()> {
        let inode = self
            .handles
            .remove(&handle)
            .ok_or_else(|| anyhow::anyhow!("bad descriptor"))?;
        self.collect(inode)
    }

    fn unlink(&mut self, name: &str) -> Result<()> {
        let inode = self.named(name)?.ok_or_else(|| anyhow::anyhow!("not found"))?;
        self.tx().open_table(NAMES)?.remove(name)?;
        self.dirty();
        self.collect(inode)
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        ensure!(
            !to.is_empty() && to.len() <= 255 && !to.contains('/') && to != "." && to != "..",
            "invalid name"
        );
        let inode = self.named(from)?.ok_or_else(|| anyhow::anyhow!("not found"))?;
        if from == to {
            return Ok(());
        }
        let replaced = self.named(to)?;
        {
            let mut names = self.tx().open_table(NAMES)?;
            names.insert(to, inode)?;
            names.remove(from)?;
        }
        self.dirty();
        if let Some(old) = replaced {
            self.collect(old)?;
        }
        Ok(())
    }

    // Called only when no descriptors survive: restart or whole-instance rejection.
    fn recover(&mut self) -> Result<()> {
        let entries: Vec<_> = self
            .tx()
            .open_table(LENGTHS)?
            .iter()?
            .map(|entry| {
                let (id, len) = entry?;
                Ok((id.value().parse::<u64>()?, len.value()))
            })
            .collect::<Result<_>>()?;
        self.bytes = entries.iter().map(|(_, len)| len).sum();
        self.files = entries.len() as u64;
        for (id, _) in entries {
            self.next_inode = self.next_inode.max(id + 1);
            self.collect(id)?;
        }
        ensure!(
            self.bytes <= self.limits.bytes && self.files <= self.limits.files,
            "existing volume exceeds quota"
        );
        Ok(())
    }

    fn finish(&mut self, accept: bool) -> Result<()> {
        self.backend.end(accept)?;
        self.backend.begin()?;
        if !accept {
            self.handles.clear();
            self.recover()?;
        }
        Ok(())
    }
}

fn checks(kind: &str, root: &Path) -> Result<()> {
    let limits = Limits {
        bytes: 20000,
        files: 3,
        handles: 3,
        cache: 65536,
    };
    let mut v = Volume::open(kind, root, limits)?;
    let a = v.create("a")?;
    let original = payload(9001, 5);
    v.write(a, 0, &original)?;
    v.finish(true)?;
    let alias = v.open_file("a")?;
    let temp = v.create("temp")?;
    v.write(temp, 0, b"new")?;
    ensure!(v.open_file("a").is_err(), "descriptor limit");
    v.rename("temp", "a")?;
    ensure!(
        v.read(a)? == original && v.read(alias)? == original,
        "replace retargeted open descriptor"
    );
    v.write(alias, 4095, b"XY")?;
    let mut old = original.clone();
    old[4095..4097].copy_from_slice(b"XY");
    ensure!(v.read(a)? == old && v.read(temp)? == b"new", "inode alias visibility");
    v.finish(true)?;
    ensure!(v.read(a)? == old, "commit invalidated descriptor");
    v.unlink("a")?;
    ensure!(
        v.named("a")?.is_none() && v.read(temp)? == b"new",
        "unlink lost held data"
    );
    v.close(alias)?;
    ensure!(v.files == 2 && v.bytes == 9004, "released still-open inode");
    v.close(a)?;
    ensure!(v.files == 1 && v.bytes == 3, "unlinked quota not reclaimed");
    v.close(temp)?;
    ensure!(v.files == 0 && v.bytes == 0);
    v.finish(false)?;
    ensure!(v.read(a).is_err() && v.read(temp).is_err(), "reject retained handles");
    let restored = v.open_file("a")?;
    ensure!(v.read(restored)? == b"new" && v.files == 1, "reject/recovery namespace");
    v.resize(restored, 2)?;
    v.resize(restored, 8195)?;
    let mut expected = vec![0; 8195];
    expected[..2].copy_from_slice(b"ne");
    ensure!(v.read(restored)? == expected, "truncate exposed old tail");
    v.write(restored, 20000, &[])?;
    ensure!(v.bytes == 8195, "empty write grew file");
    v.resize(restored, 20000)?;
    ensure!(v.resize(restored, 20001).is_err());
    ensure!(v.write(restored, usize::MAX, b"x").is_err());
    ensure!(v.write(restored, 20000, b"x").is_err());
    ensure!(
        v.bytes == 20000 && v.len(v.inode(restored)?)? == 20000,
        "failed growth mutated file"
    );
    v.resize(restored, 2)?;
    let b = v.create("b")?;
    let c = v.create("c")?;
    v.close(c)?;
    ensure!(v.create("d").is_err() && v.named("d")?.is_none(), "file limit");
    v.unlink("b")?;
    ensure!(v.create("d").is_err(), "held inode bypassed file quota");
    v.close(b)?;
    let d = v.create("d")?;
    v.write(d, 0, b"persist")?;
    v.rename("d", "d")?;
    v.close(restored)?;
    v.unlink("d")?;
    v.finish(true)?; // A committed unlinked-but-held inode must disappear on restart.
    drop(v);
    let mut v = Volume::open(kind, root, limits)?;
    ensure!(v.files == 2 && v.bytes == 2, "restart leaked unlinked inode");
    let a = v.open_file("a")?;
    ensure!(v.read(a)? == b"ne");
    v.rename("a", "moved")?;
    v.unlink("c")?;
    let doomed = v.create("doomed")?;
    v.write(doomed, 0, b"discard")?;
    v.finish(false)?;
    ensure!(v.named("a")?.is_some() && v.named("c")?.is_some());
    ensure!(v.named("moved")?.is_none() && v.named("doomed")?.is_none());
    ensure!(v.read(doomed).is_err() && v.files == 2 && v.bytes == 2);
    v.backend.audit()?;
    v.finish(true)?;
    drop(v);
    let mut v = Volume::open(kind, &root.join("quota"), Limits { bytes: 6, ..limits })?;
    let a = v.create("a")?;
    let b = v.create("b")?;
    v.write(a, 0, b"abc")?;
    v.write(b, 0, b"abc")?;
    ensure!(
        v.bytes == 6 && v.write(b, 3, b"x").is_err(),
        "dedup bypassed logical quota"
    );
    v.unlink("a")?;
    ensure!(v.write(b, 3, b"x").is_err(), "unlink released held bytes");
    v.close(a)?;
    v.write(b, 3, b"x")?;
    ensure!(v.read(b)? == b"abcx" && v.bytes == 4);
    v.finish(false)?;
    ensure!(v.bytes == 0 && v.files == 0);
    drop(v);
    let meta = root.join("metadata-only");
    let mut v = Volume::open(kind, &meta, limits)?;
    v.create("name")?;
    v.finish(true)?;
    v.rename("name", "renamed")?;
    v.finish(true)?;
    drop(v);
    let mut v = Volume::open(kind, &meta, limits)?;
    ensure!(
        v.named("name")?.is_none() && v.named("renamed")?.is_some(),
        "metadata-only rename lost"
    );
    let held = v.open_file("renamed")?;
    v.unlink("renamed")?;
    v.finish(true)?;
    ensure!(v.read(held)?.is_empty());
    drop(v);
    let v = Volume::open(kind, &meta, limits)?;
    ensure!(v.files == 0, "metadata-only unlink lost");
    Ok(())
}

fn crash(kind: &str, root: &Path, child_stage: Option<&str>) -> Result<()> {
    let limits = Limits {
        bytes: 65536,
        files: 4,
        handles: 4,
        cache: 65536,
    };
    if let Some(stage) = child_stage {
        let mut v = Volume::open(kind, root, limits)?;
        let _held = v.open_file("a")?;
        let h = v.create("temp")?;
        v.write(h, 0, b"new-a")?;
        v.close(h)?;
        v.rename("temp", "a")?;
        let b = v.open_file("b")?;
        v.write(b, 0, b"new-b")?;
        if stage == "after" {
            v.finish(true)?;
        }
        println!("ready");
        std::io::stdout().flush()?;
        if stage == "racing" {
            v.finish(true)?;
        }
        loop {
            std::thread::park();
        }
    }
    for stage in ["before", "racing", "after"] {
        let root = root.join(stage);
        let mut v = Volume::open(kind, &root, limits)?;
        let a = v.create("a")?;
        v.write(a, 0, b"old-a")?;
        let b = v.create("b")?;
        v.write(b, 0, b"old-b")?;
        v.finish(true)?;
        drop(v);
        let mut child = Command::new(std::env::current_exe()?)
            .args(["inode-child", kind])
            .arg(&root)
            .arg(stage)
            .stdout(Stdio::piped())
            .spawn()?;
        let mut ready = String::new();
        BufReader::new(child.stdout.take().expect("stdout")).read_line(&mut ready)?;
        ensure!(ready.trim() == "ready");
        child.kill()?;
        ensure!(child.wait()?.signal() == Some(9));
        let mut v = Volume::open(kind, &root, limits)?;
        let a = v.open_file("a")?;
        let b = v.open_file("b")?;
        let old = v.read(a)? == b"old-a" && v.read(b)? == b"old-b";
        let new = v.read(a)? == b"new-a" && v.read(b)? == b"new-b";
        ensure!(
            match stage {
                "before" => old,
                "after" => new,
                _ => old || new,
            },
            "torn namespace commit"
        );
        ensure!(
            v.files == 2 && v.bytes == 10 && v.named("temp")?.is_none(),
            "crash leaked orphan"
        );
        v.backend.audit()?;
        v.finish(true)?;
    }
    Ok(())
}

fn rss() -> Result<u64> {
    let status = fs::read_to_string("/proc/self/status")?;
    Ok(status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .expect("RSS")
        .split_whitespace()
        .next()
        .expect("RSS value")
        .parse::<u64>()?
        * 1024)
}

fn benchmark(kind: &str, root: &Path, cache: usize, workload: &str, size: usize, round: usize) -> Result<()> {
    let limits = Limits {
        bytes: 64 << 20,
        files: 64,
        handles: 8,
        cache,
    };
    let mut v = Volume::open(kind, root, limits)?;
    let h = v.create("value")?;
    let initial = payload(size, 0);
    v.write(h, 0, &initial)?;
    v.close(h)?;
    v.finish(true)?;
    let count = 40;
    let mut times = Vec::new();
    let mut expected = initial;
    for i in 0..count {
        let data = payload(if workload == "page-write" { PAGE } else { size }, i + 1);
        let start = Instant::now();
        if workload == "read" {
            let h = v.open_file("value")?;
            let result = v.read(h)?;
            v.close(h)?;
            v.finish(true)?;
            times.push(start.elapsed().as_secs_f64() * 1e6);
            ensure!(result == expected);
        } else {
            if workload == "page-write" {
                let h = v.open_file("value")?;
                v.write(h, 4093, &data)?;
                v.close(h)?;
            } else {
                let h = v.create("temp")?;
                v.write(h, 0, &data)?;
                v.close(h)?;
                v.rename("temp", "value")?;
            }
            v.finish(true)?;
            times.push(start.elapsed().as_secs_f64() * 1e6);
            if workload == "page-write" {
                expected[4093..4093 + PAGE].copy_from_slice(&data);
            } else {
                expected = data;
            }
        }
    }
    drop(v);
    let mut v = Volume::open(kind, root, limits)?;
    let h = v.open_file("value")?;
    ensure!(v.read(h)? == expected && v.files == 1 && v.bytes == size as u64);
    v.backend.audit()?;
    times.sort_by(f64::total_cmp);
    println!(
        "{}",
        serde_json::json!({"kind":kind,"cache":cache,"workload":workload,"size":size,"round":round,"count":count,"mean_us":times.iter().sum::<f64>()/count as f64,"p50_us":times[20],"p99_us":times[39],"allocated_bytes":storage(root)?.0})
    );
    Ok(())
}

pub(super) fn run(args: &[String]) -> Result<()> {
    if args[0] == "inode-child" {
        return crash(&args[1], Path::new(&args[2]), Some(&args[3]));
    }
    let root = std::env::current_dir()?.join(format!("target/fs-inodes-{}", std::process::id()));
    ensure!(!root.exists());
    fs::create_dir_all(&root)?;
    if args[0] == "density" {
        let kind = &args[1];
        let cache: usize = args[2].parse()?;
        let count: usize = args[3].parse()?;
        let size: usize = args[4].parse()?;
        let before = rss()?;
        let mut volumes = Vec::new();
        let limits = Limits {
            bytes: 4 << 20,
            files: 8,
            handles: 8,
            cache,
        };
        let start = Instant::now();
        for i in 0..count {
            let mut v = Volume::open(kind, &root.join(i.to_string()), limits)?;
            let h = v.create("value")?;
            v.write(h, 0, &payload(size, i as u64))?;
            ensure!(v.read(h)? == payload(size, i as u64));
            volumes.push((v, h));
        }
        let dirty = rss()?;
        for (i, (v, h)) in volumes.iter_mut().enumerate() {
            v.finish(true)?;
            ensure!(v.read(*h)? == payload(size, i as u64));
        }
        let resident = rss()?;
        println!(
            "{}",
            serde_json::json!({"kind":kind,"cache":cache,"objects":count,"size":size,"rss_before":before,"rss_dirty":dirty,"rss_after":resident,"rss_per_object":resident.saturating_sub(before)/count as u64,"dirty_rss_per_object":dirty.saturating_sub(before)/count as u64,"elapsed_ms":start.elapsed().as_millis(),"allocated_bytes":storage(&root)?.0})
        );
        drop(volumes);
    } else {
        for kind in ["pages", "hashed"] {
            checks(kind, &root.join(kind))?;
            crash(kind, &root.join(format!("crash-{kind}")), None)?;
        }
        eprintln!(
            "PASS held descriptors, rename/unlink, rejection, sparse truncate, byte/file/handle quotas and orphan recovery"
        );
        if args[0] == "inode-bench" {
            for round in 0..3 {
                for cache in [65536, 1048576] {
                    for (workload, size) in [
                        ("replace", 128),
                        ("replace", 16384),
                        ("replace", 1048576),
                        ("read", 1048576),
                        ("page-write", 1048576),
                    ] {
                        for i in 0..2 {
                            let kind = ["pages", "hashed"][(round + i) % 2];
                            benchmark(
                                kind,
                                &root.join(format!("{round}-{cache}-{workload}-{size}-{kind}")),
                                cache,
                                workload,
                                size,
                                round,
                            )?;
                        }
                    }
                }
            }
        }
    }
    fs::remove_dir_all(root)?;
    Ok(())
}
