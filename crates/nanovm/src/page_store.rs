//! Transactional inode namespace with directly addressed 4 KiB pages.
//!
//! One candidate write transaction owns both roots. Dropping it rejects the candidate;
//! committing publishes it without invalidating retained inode identities. Storage
//! failures poison the candidate, including failures during deferred orphan collection.

use anyhow::{Context, Result, anyhow, ensure};
use redb::{Database, Durability, ReadableTable, StorageBackend, TableDefinition, WriteTransaction};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Included, Unbounded};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use wasmtime_wasi::p3::bindings::clocks::system_clock::Instant;
use wasmtime_wasi::p3::bindings::filesystem::types::{
    DescriptorStat, DescriptorType, DirectoryEntry, ErrorCode, NewTimestamp,
};
use wasmtime_wasi::p3::filesystem::{FilesystemError, FilesystemResult};

pub(crate) const DATA: u64 = 1;
pub(crate) const TMP: u64 = 2;
const PAGE: usize = 4096;
const MAX_PATH: usize = 4096;
const MAX_NAME: usize = 255;
const MAX_BATCH: usize = 256;
const NODES: TableDefinition<u64, &[u8]> = TableDefinition::new("inodes");
const NAMES: TableDefinition<(u64, &str), u64> = TableDefinition::new("names");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const PAGES: TableDefinition<(u64, u64), &[u8]> = TableDefinition::new("pages");

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Stamp {
    seconds: i64,
    nanoseconds: u32,
}

impl Stamp {
    fn now() -> Self {
        let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        Self {
            seconds: elapsed.as_secs().min(i64::MAX as u64) as i64,
            nanoseconds: elapsed.subsec_nanos(),
        }
    }

    fn update(self, value: NewTimestamp, now: Self) -> Result<Self> {
        match value {
            NewTimestamp::NoChange => Ok(self),
            NewTimestamp::Now => Ok(now),
            NewTimestamp::Timestamp(value) => {
                if value.nanoseconds >= 1_000_000_000 {
                    return Err(ErrorCode::Invalid.into());
                }
                let seconds = Duration::from_secs(value.seconds.unsigned_abs());
                let time = if value.seconds < 0 {
                    UNIX_EPOCH.checked_sub(seconds)
                } else {
                    UNIX_EPOCH.checked_add(seconds)
                };
                time.and_then(|t| t.checked_add(Duration::from_nanos(u64::from(value.nanoseconds))))
                    .ok_or(ErrorCode::Overflow)?;
                Ok(Self {
                    seconds: value.seconds,
                    nanoseconds: value.nanoseconds,
                })
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Node {
    root: u64,
    parent: u64,
    name: String,
    directory: bool,
    linked: bool,
    size: u64,
    atime: Stamp,
    mtime: Stamp,
    ctime: Stamp,
}

impl Node {
    fn kind(&self) -> DescriptorType {
        if self.directory {
            DescriptorType::Directory
        } else {
            DescriptorType::RegularFile
        }
    }
}

pub(crate) struct Volume {
    // Drop the candidate before dropping the database.
    tx: Option<WriteTransaction>,
    db: Database,
    held: BTreeMap<u64, u64>,
    poisoned: Option<String>,
    dirty: Cell<bool>,
    bytes: u64,
    files: u64,
    max_bytes: u64,
    max_files: u64,
}

impl Volume {
    pub(crate) fn open(
        storage: impl StorageBackend,
        max_bytes: u64,
        max_files: u64,
        cache_bytes: usize,
    ) -> Result<Self> {
        let db = Database::builder()
            .set_cache_size(cache_bytes)
            .create_with_backend(storage)?;
        let tx = Self::begin(&db)?;
        tx.open_table(NODES)?;
        tx.open_table(NAMES)?;
        tx.open_table(META)?;
        tx.open_table(PAGES)?;
        let mut v = Self {
            tx: Some(tx),
            db,
            held: BTreeMap::new(),
            poisoned: None,
            dirty: Cell::new(false),
            bytes: 0,
            files: 0,
            max_bytes,
            max_files,
        };
        if v.tx().open_table(META)?.get("next")?.is_none() {
            v.tx().open_table(META)?.insert("next", 3)?;
            for id in [DATA, TMP] {
                let now = Stamp::now();
                v.put_node(
                    id,
                    &Node {
                        root: id,
                        parent: id,
                        name: String::new(),
                        directory: true,
                        linked: true,
                        size: 0,
                        atime: now,
                        mtime: now,
                        ctime: now,
                    },
                )?;
            }
        }
        // Stream the inode inventory, never materialize the namespace in memory.
        let mut after = TMP;
        loop {
            let next = v
                .tx()
                .open_table(NODES)?
                .range((Excluded(after), Included(u64::MAX)))?
                .next()
                .transpose()?
                .map(|(id, data)| (id.value(), data.value().to_vec()));
            let Some((id, data)) = next else { break };
            let node: Node = serde_json::from_slice(&data)?;
            after = id;
            v.bytes = v.bytes.checked_add(node.size).context("logical byte count overflow")?;
            v.files = v.files.checked_add(1).context("inode count overflow")?;
            if node.root == TMP || !node.linked {
                if node.linked {
                    v.tx().open_table(NAMES)?.remove((node.parent, node.name.as_str()))?;
                }
                v.destroy(id, &node)?;
            }
        }
        ensure!(
            v.bytes <= max_bytes && v.files <= max_files,
            "existing volume exceeds logical quota"
        );
        // Recovery is itself atomic and durable before any guest operation is allowed.
        v.commit()?;
        Ok(v)
    }

    fn begin(db: &Database) -> Result<WriteTransaction> {
        let mut tx = db.begin_write()?;
        tx.set_durability(Durability::Immediate)?;
        tx.set_two_phase_commit(true);
        Ok(tx)
    }

    fn tx(&self) -> &WriteTransaction {
        self.tx.as_ref().expect("healthy volume has a transaction")
    }

    fn run<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> FilesystemResult<T> {
        if let Some(error) = &self.poisoned {
            return Err(FilesystemError::trap(std::io::Error::other(format!(
                "volume poisoned: {error}"
            ))));
        }
        f(self).map_err(|error| match error.downcast::<ErrorCode>() {
            Ok(code) => code.into(),
            Err(error) => {
                self.poisoned = Some(format!("{error:#}"));
                FilesystemError::trap(std::io::Error::other(error))
            }
        })
    }

    fn node(&self, id: u64) -> Result<Node> {
        let table = self.tx().open_table(NODES)?;
        let data = table.get(id)?.ok_or(ErrorCode::BadDescriptor)?;
        Ok(serde_json::from_slice(data.value())?)
    }

    fn put_node(&self, id: u64, node: &Node) -> Result<()> {
        self.dirty.set(true);
        self.tx()
            .open_table(NODES)?
            .insert(id, serde_json::to_vec(node)?.as_slice())?;
        Ok(())
    }

    fn directory(&self, id: u64, mutable: bool) -> Result<Node> {
        let node = self.node(id)?;
        if !node.directory {
            return Err(ErrorCode::NotDirectory.into());
        }
        if mutable && !node.linked {
            return Err(ErrorCode::NoEntry.into());
        }
        Ok(node)
    }

    fn file(&self, id: u64) -> Result<Node> {
        let node = self.node(id)?;
        if node.directory {
            return Err(ErrorCode::IsDirectory.into());
        }
        Ok(node)
    }

    fn named(&self, parent: u64, name: &str) -> Result<Option<u64>> {
        Ok(self.tx().open_table(NAMES)?.get((parent, name))?.map(|v| v.value()))
    }

    fn check_path(path: &str) -> Result<()> {
        if path.len() > MAX_PATH {
            return Err(ErrorCode::NameTooLong.into());
        }
        if path.starts_with('/') {
            return Err(ErrorCode::NotPermitted.into());
        }
        if path.contains('\0') {
            return Err(ErrorCode::Invalid.into());
        }
        if path.is_empty() {
            return Err(ErrorCode::NoEntry.into());
        }
        if path.split('/').any(|part| part.len() > MAX_NAME) {
            return Err(ErrorCode::NameTooLong.into());
        }
        Ok(())
    }

    fn walk(&self, dir: u64, path: &str) -> Result<u64> {
        Self::check_path(path)?;
        self.directory(dir, false)?;
        let mut current = dir;
        let mut depth = 0;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            let node = self.directory(current, false)?;
            match part {
                "." => {}
                ".." => {
                    if depth == 0 {
                        return Err(ErrorCode::NotPermitted.into());
                    }
                    current = node.parent;
                    depth -= 1;
                }
                name => {
                    current = self.named(current, name)?.ok_or(ErrorCode::NoEntry)?;
                    depth += 1;
                }
            }
        }
        if path.ends_with('/') {
            self.directory(current, false)?;
        }
        Ok(current)
    }

    fn parent<'a>(&self, dir: u64, path: &'a str) -> Result<(u64, &'a str)> {
        Self::check_path(path)?;
        let path = path.trim_end_matches('/');
        let (parent, name) = match path.rsplit_once('/') {
            Some((prefix, name)) => (self.walk(dir, prefix)?, name),
            None => (dir, path),
        };
        if matches!(name, "" | "." | "..") {
            return Err(ErrorCode::Invalid.into());
        }
        self.directory(parent, true)?;
        Ok((parent, name))
    }

    fn touch(&self, id: u64) -> Result<()> {
        let mut node = self.node(id)?;
        node.mtime = Stamp::now();
        node.ctime = node.mtime;
        self.put_node(id, &node)
    }

    pub(crate) fn stat(&mut self, id: u64) -> FilesystemResult<DescriptorStat> {
        self.run(|v| {
            let n = v.node(id)?;
            Ok(DescriptorStat {
                type_: n.kind(),
                link_count: u64::from(n.linked),
                size: n.size,
                data_access_timestamp: Some(Instant {
                    seconds: n.atime.seconds,
                    nanoseconds: n.atime.nanoseconds,
                }),
                data_modification_timestamp: Some(Instant {
                    seconds: n.mtime.seconds,
                    nanoseconds: n.mtime.nanoseconds,
                }),
                status_change_timestamp: Some(Instant {
                    seconds: n.ctime.seconds,
                    nanoseconds: n.ctime.nanoseconds,
                }),
            })
        })
    }

    pub(crate) fn resolve(&mut self, dir: u64, path: &str) -> FilesystemResult<u64> {
        self.run(|v| v.walk(dir, path))
    }

    pub(crate) fn create(&mut self, dir: u64, path: &str, directory: bool) -> FilesystemResult<u64> {
        self.run(|v| {
            let (parent, name) = v.parent(dir, path)?;
            if v.named(parent, name)?.is_some() {
                return Err(ErrorCode::Exist.into());
            }
            if path.ends_with('/') && !directory {
                return Err(ErrorCode::NotDirectory.into());
            }
            if v.files >= v.max_files {
                return Err(ErrorCode::Quota.into());
            }
            let root = v.node(parent)?.root;
            let id = v
                .tx()
                .open_table(META)?
                .get("next")?
                .context("missing inode allocator")?
                .value();
            let next = id.checked_add(1).ok_or(ErrorCode::Overflow)?;
            let now = Stamp::now();
            v.put_node(
                id,
                &Node {
                    root,
                    parent,
                    name: name.to_owned(),
                    directory,
                    linked: true,
                    size: 0,
                    atime: now,
                    mtime: now,
                    ctime: now,
                },
            )?;
            v.tx().open_table(META)?.insert("next", next)?;
            v.tx().open_table(NAMES)?.insert((parent, name), id)?;
            v.files += 1;
            v.touch(parent)?;
            Ok(id)
        })
    }

    fn empty(&self, id: u64) -> Result<bool> {
        let table = self.tx().open_table(NAMES)?;
        let first = table.range((id, "")..)?.next().transpose()?;
        Ok(first.is_none_or(|(key, _)| key.value().0 != id))
    }

    fn orphan(&mut self, id: u64, mut node: Node) -> Result<()> {
        node.linked = false;
        node.ctime = Stamp::now();
        self.put_node(id, &node)?;
        self.collect(id)
    }

    pub(crate) fn unlink(&mut self, dir: u64, path: &str, directory: bool) -> FilesystemResult<()> {
        self.run(|v| {
            let (parent, name) = v.parent(dir, path)?;
            let id = v.named(parent, name)?.ok_or(ErrorCode::NoEntry)?;
            let node = v.node(id)?;
            if (directory || path.ends_with('/')) && !node.directory {
                return Err(ErrorCode::NotDirectory.into());
            }
            if !directory && node.directory {
                return Err(ErrorCode::IsDirectory.into());
            }
            if node.directory && !v.empty(id)? {
                return Err(ErrorCode::NotEmpty.into());
            }
            v.tx().open_table(NAMES)?.remove((parent, name))?;
            v.orphan(id, node)?;
            v.touch(parent)
        })
    }

    pub(crate) fn rename(&mut self, from_dir: u64, from: &str, to_dir: u64, to: &str) -> FilesystemResult<()> {
        self.run(|v| {
            let (fp, fname) = v.parent(from_dir, from)?;
            let (tp, tname) = v.parent(to_dir, to)?;
            let id = v.named(fp, fname)?.ok_or(ErrorCode::NoEntry)?;
            let mut node = v.node(id)?;
            if node.root != v.node(tp)?.root {
                return Err(ErrorCode::CrossDevice.into());
            }
            if (from.ends_with('/') || to.ends_with('/')) && !node.directory {
                return Err(ErrorCode::NotDirectory.into());
            }
            if fp == tp && fname == tname {
                return Ok(());
            }
            if node.directory {
                let mut ancestor = tp;
                loop {
                    if ancestor == id {
                        return Err(ErrorCode::Invalid.into());
                    }
                    let parent = v.node(ancestor)?.parent;
                    if ancestor == parent {
                        break;
                    }
                    ancestor = parent;
                }
            }
            let replaced = v
                .named(tp, tname)?
                .map(|old| Ok::<_, anyhow::Error>((old, v.node(old)?)))
                .transpose()?;
            if let Some((old, target)) = &replaced {
                if node.directory && !target.directory {
                    return Err(ErrorCode::NotDirectory.into());
                }
                if !node.directory && target.directory {
                    return Err(ErrorCode::IsDirectory.into());
                }
                if target.directory && !v.empty(*old)? {
                    return Err(ErrorCode::NotEmpty.into());
                }
            }
            v.tx().open_table(NAMES)?.remove((fp, fname))?;
            v.tx().open_table(NAMES)?.insert((tp, tname), id)?;
            node.parent = tp;
            node.name = tname.to_owned();
            node.ctime = Stamp::now();
            v.put_node(id, &node)?;
            if let Some((old, target)) = replaced {
                v.orphan(old, target)?;
            }
            v.touch(fp)?;
            if fp != tp {
                v.touch(tp)?;
            }
            Ok(())
        })
    }

    fn page(pages: &redb::Table<(u64, u64), &[u8]>, id: u64, index: u64, out: &mut [u8; PAGE]) -> Result<()> {
        out.fill(0);
        if let Some(blob) = pages.get((id, index))? {
            ensure!(blob.value().len() == PAGE, "invalid page length");
            out.copy_from_slice(blob.value());
        }
        Ok(())
    }

    fn put_page(pages: &mut redb::Table<(u64, u64), &[u8]>, id: u64, index: u64, bytes: &[u8; PAGE]) -> Result<()> {
        if pages.get((id, index))?.is_some_and(|old| old.value() == bytes) {
            return Ok(());
        }
        // Holes need no physical page, but their logical length is still charged.
        if bytes.iter().all(|&b| b == 0) {
            pages.remove((id, index))?;
        } else {
            pages.insert((id, index), bytes.as_slice())?;
        }
        Ok(())
    }

    fn truncate_pages(&self, id: u64, size: u64) -> Result<()> {
        let first = size.div_ceil(PAGE as u64);
        let mut pages = self.tx().open_table(PAGES)?;
        pages.retain_in((id, first)..=(id, u64::MAX), |_, _| false)?;
        let tail = (size % PAGE as u64) as usize;
        if tail != 0 {
            let mut page = [0; PAGE];
            Self::page(&pages, id, size / PAGE as u64, &mut page)?;
            page[tail..].fill(0);
            Self::put_page(&mut pages, id, size / PAGE as u64, &page)?;
        }
        Ok(())
    }

    fn destroy(&mut self, id: u64, node: &Node) -> Result<()> {
        self.dirty.set(true);
        self.truncate_pages(id, 0)?;
        self.tx().open_table(NODES)?.remove(id)?;
        self.bytes -= node.size;
        self.files -= 1;
        Ok(())
    }

    fn collect(&mut self, id: u64) -> Result<()> {
        if self.held.contains_key(&id) {
            return Ok(());
        }
        let node = self.node(id)?;
        if !node.linked {
            self.destroy(id, &node)?;
        }
        Ok(())
    }

    pub(crate) fn read(&mut self, id: u64, offset: u64, out: &mut [u8]) -> FilesystemResult<usize> {
        self.run(|v| {
            let node = v.file(id)?;
            let count = node.size.saturating_sub(offset).min(out.len() as u64) as usize;
            if count == 0 {
                return Ok(0);
            }
            let end = offset + count as u64;
            out[..count].fill(0);
            let pages = v.tx().open_table(PAGES)?;
            for entry in pages.range((id, offset / PAGE as u64)..=(id, (end - 1) / PAGE as u64))? {
                let (key, blob) = entry?;
                ensure!(blob.value().len() == PAGE, "invalid page length");
                let start = key.value().1 * PAGE as u64;
                let within = offset.saturating_sub(start) as usize;
                let last = (end - start).min(PAGE as u64) as usize;
                let destination = (start + within as u64 - offset) as usize;
                out[destination..destination + last - within].copy_from_slice(&blob.value()[within..last]);
            }
            Ok(count)
        })
    }

    pub(crate) fn write(&mut self, id: u64, offset: Option<u64>, bytes: &[u8]) -> FilesystemResult<usize> {
        self.run(|v| {
            let mut node = v.file(id)?;
            let offset = offset.unwrap_or(node.size);
            if bytes.is_empty() {
                return Ok(0);
            }
            offset.checked_add(bytes.len() as u64).ok_or(ErrorCode::Overflow)?;
            let maximum = node.size.saturating_add(v.max_bytes.saturating_sub(v.bytes));
            let count = maximum.saturating_sub(offset).min(bytes.len() as u64) as usize;
            if count == 0 {
                return Err(ErrorCode::InsufficientSpace.into());
            }
            let size = node.size.max(offset + count as u64);
            let mut done = 0;
            let mut page = [0; PAGE];
            let mut pages = v.tx().open_table(PAGES)?;
            while done < count {
                let at = offset + done as u64;
                let within = (at % PAGE as u64) as usize;
                let n = (count - done).min(PAGE - within);
                if within != 0 || n != PAGE {
                    Self::page(&pages, id, at / PAGE as u64, &mut page)?;
                }
                page[within..within + n].copy_from_slice(&bytes[done..done + n]);
                Self::put_page(&mut pages, id, at / PAGE as u64, &page)?;
                done += n;
            }
            drop(pages);
            v.bytes += size - node.size;
            node.size = size;
            node.mtime = Stamp::now();
            node.ctime = node.mtime;
            v.put_node(id, &node)?;
            Ok(count)
        })
    }

    pub(crate) fn resize(&mut self, id: u64, size: u64) -> FilesystemResult<()> {
        self.run(|v| {
            let mut node = v.file(id)?;
            if size.saturating_sub(node.size) > v.max_bytes.saturating_sub(v.bytes) {
                return Err(ErrorCode::InsufficientSpace.into());
            }
            if size < node.size {
                v.truncate_pages(id, size)?;
            }
            v.bytes = v.bytes - node.size + size;
            node.size = size;
            node.mtime = Stamp::now();
            node.ctime = node.mtime;
            v.put_node(id, &node)
        })
    }

    pub(crate) fn entries(
        &mut self,
        id: u64,
        after: Option<&str>,
        limit: usize,
    ) -> FilesystemResult<Vec<DirectoryEntry>> {
        self.run(|v| {
            v.directory(id, false)?;
            if after.is_some_and(|name| name.len() > MAX_NAME) {
                return Err(ErrorCode::NameTooLong.into());
            }
            let table = v.tx().open_table(NAMES)?;
            let start = after.map_or(Included((id, "")), |name| Excluded((id, name)));
            let mut result = Vec::new();
            for item in table.range((start, Unbounded))?.take(limit.min(MAX_BATCH)) {
                let (key, inode) = item?;
                if key.value().0 != id {
                    break;
                }
                result.push(DirectoryEntry {
                    name: key.value().1.to_owned(),
                    type_: v.node(inode.value())?.kind(),
                });
            }
            Ok(result)
        })
    }

    pub(crate) fn set_times(&mut self, id: u64, atime: NewTimestamp, mtime: NewTimestamp) -> FilesystemResult<()> {
        self.run(|v| {
            let mut node = v.node(id)?;
            let now = Stamp::now();
            let atime = node.atime.update(atime, now)?;
            let mtime = node.mtime.update(mtime, now)?;
            node.atime = atime;
            node.mtime = mtime;
            node.ctime = now;
            v.put_node(id, &node)
        })
    }

    /// The adapter retains once per live resource and releases exactly once on drop.
    pub(crate) fn retain(&mut self, id: u64) {
        let count = self.held.entry(id).or_default();
        match count.checked_add(1) {
            Some(next) => *count = next,
            None => self.poisoned = Some("inode retain count overflow".into()),
        }
    }

    pub(crate) fn release(&mut self, id: u64) {
        if let Some(count) = self.held.get_mut(&id) {
            *count -= 1;
            if *count == 0 {
                self.held.remove(&id);
                let _ = self.run(|v| v.collect(id));
            }
        }
    }

    pub(crate) fn commit(&mut self) -> Result<()> {
        if let Some(error) = &self.poisoned {
            return Err(anyhow!("volume poisoned: {error}"));
        }
        if !self.dirty.get() {
            return Ok(());
        }
        let result = (|| {
            self.tx.take().context("missing candidate transaction")?.commit()?;
            self.dirty.set(false);
            self.tx = Some(Self::begin(&self.db)?);
            Ok(())
        })();
        if let Err(error) = &result {
            self.poisoned = Some(format!("{error:#}"));
        }
        result
    }

    #[cfg(test)]
    pub(crate) fn usage(&self) -> (u64, u64) {
        (self.bytes, self.files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::backends::InMemoryBackend;
    use std::io;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[derive(Clone, Debug, Default)]
    struct Storage {
        bytes: Arc<InMemoryBackend>,
        fail_sync: Arc<AtomicBool>,
        fail_reads: Arc<AtomicBool>,
    }

    impl StorageBackend for Storage {
        fn len(&self) -> io::Result<u64> {
            self.bytes.len()
        }
        fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
            if self.fail_reads.load(Ordering::Relaxed) {
                Err(io::Error::other("injected read failure"))
            } else {
                self.bytes.read(offset, out)
            }
        }
        fn write(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
            self.bytes.write(offset, bytes)
        }
        fn set_len(&self, len: u64) -> io::Result<()> {
            self.bytes.set_len(len)
        }
        fn sync_data(&self) -> io::Result<()> {
            if self.fail_sync.load(Ordering::Relaxed) {
                Err(io::Error::other("injected sync failure"))
            } else {
                self.bytes.sync_data()
            }
        }
    }

    fn open(storage: &Storage, bytes: u64, files: u64) -> Volume {
        Volume::open(storage.clone(), bytes, files, 65536).unwrap()
    }

    fn code<T: std::fmt::Debug>(result: FilesystemResult<T>, expected: ErrorCode) {
        assert_eq!(
            format!("{:?}", result.unwrap_err().downcast().unwrap()),
            format!("{expected:?}")
        );
    }

    fn contents(v: &mut Volume, id: u64) -> Vec<u8> {
        let mut bytes = vec![0; v.stat(id).unwrap().size as usize];
        assert_eq!(v.read(id, 0, &mut bytes).unwrap(), bytes.len());
        bytes
    }

    #[test]
    fn read_only_acceptance_needs_no_storage_writes() {
        let storage = Storage::default();
        let mut v = open(&storage, 100, 10);
        let id = v.create(DATA, "value", false).unwrap();
        v.write(id, None, b"committed").unwrap();
        v.commit().unwrap();
        let mut before = vec![0; storage.len().unwrap() as usize];
        storage.read(0, &mut before).unwrap();
        assert_eq!(v.resolve(DATA, "value").unwrap(), id);
        assert_eq!(contents(&mut v, id), b"committed");
        v.commit().unwrap();
        let mut after = vec![0; storage.len().unwrap() as usize];
        storage.read(0, &mut after).unwrap();
        assert_eq!(before, after, "read-only acceptance rewrote the database");
    }

    #[test]
    fn nested_paths_are_confined_to_descriptor() {
        let mut v = open(&Storage::default(), 100, 20);
        let a = v.create(DATA, "a", true).unwrap();
        let b = v.create(a, "b", true).unwrap();
        let f = v.create(DATA, "a/./b/f", false).unwrap();
        assert_eq!(v.resolve(a, "b/../b//f").unwrap(), f);
        assert_eq!(v.resolve(b, ".").unwrap(), b);
        for path in ["..", "../a", "b/../../a", "/a"] {
            code(v.resolve(a, path), ErrorCode::NotPermitted);
        }
        code(v.create(b, "../escape", false), ErrorCode::NotPermitted);
        code(v.resolve(DATA, "a/b/f/.."), ErrorCode::NotDirectory);
        code(v.resolve(DATA, "a/b/f/"), ErrorCode::NotDirectory);
        code(v.create(DATA, "\0", false), ErrorCode::Invalid);
        code(v.resolve(DATA, &"x".repeat(MAX_PATH + 1)), ErrorCode::NameTooLong);
        code(v.create(DATA, &"x".repeat(MAX_NAME + 1), false), ErrorCode::NameTooLong);
        assert_eq!(v.usage(), (0, 3));
        v.commit().unwrap();
    }

    #[test]
    fn directory_renames_preserve_identity_and_reject_cycles() {
        let mut v = open(&Storage::default(), 100, 20);
        let a = v.create(DATA, "a", true).unwrap();
        let b = v.create(a, "b", true).unwrap();
        let file = v.create(b, "file", false).unwrap();
        v.retain(a);
        v.retain(b);
        code(v.rename(DATA, "a", b, "cycle"), ErrorCode::Invalid);
        code(v.rename(DATA, "a", TMP, "a"), ErrorCode::CrossDevice);
        code(v.unlink(DATA, "a", true), ErrorCode::NotEmpty);
        v.rename(DATA, "a", DATA, "moved").unwrap();
        assert_eq!(v.resolve(DATA, "moved/b/file").unwrap(), file);
        assert_eq!(v.resolve(a, "b").unwrap(), b);
        code(v.resolve(b, ".."), ErrorCode::NotPermitted);
        v.rename(DATA, "moved", DATA, "moved").unwrap();
        let empty = v.create(DATA, "empty", true).unwrap();
        v.retain(empty);
        v.rename(DATA, "moved", DATA, "empty").unwrap();
        assert_eq!(v.resolve(DATA, "empty").unwrap(), a);
        assert_eq!(v.stat(empty).unwrap().link_count, 0);
        code(v.create(empty, "lost", false), ErrorCode::NoEntry);
        code(v.rename(a, "b", empty, "lost"), ErrorCode::NoEntry);
        assert!(v.entries(empty, None, 10).unwrap().is_empty());
        v.release(empty);
        code(v.stat(empty), ErrorCode::BadDescriptor);
        code(v.rename(b, "file", DATA, "empty"), ErrorCode::IsDirectory);
        code(v.rename(a, "b", b, "file"), ErrorCode::Invalid);
        let other = v.create(DATA, "other", true).unwrap();
        code(v.rename(DATA, "other", b, "file"), ErrorCode::NotDirectory);
        code(v.rename(DATA, "other", DATA, "empty"), ErrorCode::NotEmpty);
        v.retain(other);
        v.unlink(DATA, "other", true).unwrap();
        code(v.create(other, "lost", false), ErrorCode::NoEntry);
        v.commit().unwrap();
        assert_eq!(v.resolve(a, "b/file").unwrap(), file);
    }

    #[test]
    fn shared_quota_charges_held_unlinked_files_and_directories() {
        let mut v = open(&Storage::default(), 6, 3);
        assert_eq!(v.usage(), (0, 0));
        let a = v.create(DATA, "a", false).unwrap();
        let b = v.create(TMP, "b", false).unwrap();
        let d = v.create(DATA, "d", true).unwrap();
        v.retain(a);
        v.retain(a);
        v.retain(d);
        v.write(a, None, b"abc").unwrap();
        assert_eq!(v.write(b, None, b"abcdef").unwrap(), 3);
        assert_eq!(v.usage(), (6, 3));
        code(v.create(DATA, "a", false), ErrorCode::Exist);
        code(v.create(DATA, "x", false), ErrorCode::Quota);
        code(v.resize(b, 4), ErrorCode::InsufficientSpace);
        code(v.write(b, None, b"x"), ErrorCode::InsufficientSpace);
        code(v.write(b, Some(u64::MAX), b"x"), ErrorCode::Overflow);
        assert_eq!(v.write(b, Some(u64::MAX), b"").unwrap(), 0);
        v.unlink(DATA, "a", false).unwrap();
        assert_eq!(v.stat(a).unwrap().link_count, 0);
        assert_eq!(contents(&mut v, a), b"abc");
        v.write(a, Some(1), b"X").unwrap();
        v.release(a);
        assert_eq!(v.usage(), (6, 3));
        v.release(a);
        assert_eq!(v.usage(), (3, 2));
        v.unlink(DATA, "d", true).unwrap();
        assert_eq!(v.usage(), (3, 2));
        v.release(d);
        assert_eq!(v.usage(), (3, 1));
        assert_eq!(v.write(b, None, b"defgh").unwrap(), 3);
        assert_eq!(contents(&mut v, b), b"abcdef");
    }

    #[test]
    fn replacement_keeps_old_handles_and_shared_pages_alive() {
        let mut v = open(&Storage::default(), 100_000, 20);
        let a = v.create(DATA, "a", false).unwrap();
        let b = v.create(DATA, "b", false).unwrap();
        let c = v.create(DATA, "c", false).unwrap();
        let bytes = vec![42; PAGE * 2 + 17];
        for id in [a, b, c] {
            v.write(id, None, &bytes).unwrap();
        }
        v.retain(a);
        v.rename(DATA, "b", DATA, "a").unwrap();
        v.commit().unwrap();
        assert_eq!(v.resolve(DATA, "a").unwrap(), b);
        assert_eq!(contents(&mut v, a), bytes);
        v.resize(b, 4095).unwrap();
        v.resize(b, 9000).unwrap();
        v.write(b, Some(4095), b"XY").unwrap();
        let mut expected = vec![42; 4095];
        expected.extend_from_slice(b"XY");
        expected.resize(9000, 0);
        assert_eq!(contents(&mut v, b), expected);
        let mut crossing = [0; 5];
        assert_eq!(v.read(b, 4094, &mut crossing).unwrap(), 5);
        assert_eq!(crossing, [42, b'X', b'Y', 0, 0]);
        assert_eq!(v.read(b, u64::MAX, &mut crossing).unwrap(), 0);
        v.release(a);
        v.unlink(DATA, "a", false).unwrap();
        assert_eq!(contents(&mut v, c), bytes);
        assert_eq!(v.usage(), (bytes.len() as u64, 1));
        v.unlink(DATA, "c", false).unwrap();
        assert_eq!(v.usage(), (0, 0));
    }

    #[test]
    fn accept_reject_and_restart_cleanup_are_atomic() {
        let storage = Storage::default();
        let mut v = open(&storage, 100, 20);
        let data = v.create(DATA, "persist", false).unwrap();
        v.write(data, None, b"accepted").unwrap();
        let dir = v.create(TMP, "nested", true).unwrap();
        let tmp = v.create(dir, "ephemeral", false).unwrap();
        v.write(tmp, None, b"temporary").unwrap();
        let orphan = v.create(DATA, "orphan", false).unwrap();
        v.retain(orphan);
        v.write(orphan, None, b"orphan").unwrap();
        v.unlink(DATA, "orphan", false).unwrap();
        v.commit().unwrap();
        assert_eq!(contents(&mut v, tmp), b"temporary");
        assert_eq!(contents(&mut v, orphan), b"orphan");
        v.write(data, Some(0), b"rejected").unwrap();
        v.rename(DATA, "persist", DATA, "rejected").unwrap();
        v.create(DATA, "discard", false).unwrap();
        drop(v);
        let mut v = open(&storage, 100, 20);
        assert_eq!(v.usage(), (8, 1));
        assert!(v.entries(TMP, None, 10).unwrap().is_empty());
        code(v.stat(orphan), ErrorCode::BadDescriptor);
        code(v.resolve(DATA, "rejected"), ErrorCode::NoEntry);
        code(v.resolve(DATA, "discard"), ErrorCode::NoEntry);
        assert_eq!(v.resolve(DATA, "persist").unwrap(), data);
        assert_eq!(contents(&mut v, data), b"accepted");
        assert!(v.create(DATA, "new", false).unwrap() > orphan);
        drop(v);
        let mut v = open(&storage, 100, 20);
        assert_eq!(v.usage(), (8, 1));
        assert!(v.entries(TMP, None, 10).unwrap().is_empty());
    }

    #[test]
    fn bounded_directory_cursor_and_timestamp_validation() {
        let mut v = open(&Storage::default(), 100, 400);
        for i in 0..300 {
            v.create(DATA, &format!("f{i:03}"), false).unwrap();
        }
        v.create(DATA, "\u{10ffff}suffix", false).unwrap();
        v.create(TMP, "not-data", false).unwrap();
        let first = v.entries(DATA, None, usize::MAX).unwrap();
        assert_eq!(first.len(), MAX_BATCH);
        let rest = v.entries(DATA, Some(&first.last().unwrap().name), usize::MAX).unwrap();
        assert_eq!(rest.len(), 45);
        assert_eq!(rest.last().unwrap().name, "\u{10ffff}suffix");
        assert!(v.entries(DATA, Some(&rest.last().unwrap().name), 1).unwrap().is_empty());
        assert!(v.entries(DATA, None, 0).unwrap().is_empty());
        let id = v.resolve(DATA, "f000").unwrap();
        v.set_times(
            id,
            NewTimestamp::Timestamp(Instant {
                seconds: -1,
                nanoseconds: 123,
            }),
            NewTimestamp::Now,
        )
        .unwrap();
        let before = v.stat(id).unwrap();
        assert_eq!(before.data_access_timestamp.unwrap().seconds, -1);
        code(
            v.set_times(
                id,
                NewTimestamp::Now,
                NewTimestamp::Timestamp(Instant {
                    seconds: 0,
                    nanoseconds: 1_000_000_000,
                }),
            ),
            ErrorCode::Invalid,
        );
        let after = v.stat(id).unwrap();
        assert_eq!(after.data_access_timestamp.unwrap().seconds, -1);
        let before = before.data_modification_timestamp.unwrap();
        let after = after.data_modification_timestamp.unwrap();
        assert_eq!((after.seconds, after.nanoseconds), (before.seconds, before.nanoseconds));
        v.set_times(id, NewTimestamp::NoChange, NewTimestamp::NoChange).unwrap();
        v.commit().unwrap();
    }

    #[test]
    fn deferred_collection_failure_latches_a_trap_and_rejects_candidate() {
        let storage = Storage::default();
        let mut v = Volume::open(storage.clone(), 1 << 20, 10, 4096).unwrap();
        let id = v.create(DATA, "file", false).unwrap();
        for index in 0u64..64 {
            let mut page = [17; PAGE];
            page[..8].copy_from_slice(&index.to_le_bytes());
            v.write(id, None, &page).unwrap();
        }
        v.retain(id);
        v.commit().unwrap();
        v.unlink(DATA, "file", false).unwrap();
        storage.fail_reads.store(true, Ordering::Relaxed);
        v.release(id);
        storage.fail_reads.store(false, Ordering::Relaxed);
        assert!(v.commit().is_err());
        assert!(v.stat(DATA).unwrap_err().downcast().is_err());
        drop(v);
        let mut v = open(&storage, 1 << 20, 10);
        assert_eq!(v.resolve(DATA, "file").unwrap(), id);
        assert_eq!(v.usage(), (64 * PAGE as u64, 1));
        let mut page = [0; PAGE];
        assert_eq!(v.read(id, 63 * PAGE as u64, &mut page).unwrap(), PAGE);
        assert_eq!(&page[..8], &63u64.to_le_bytes());
    }

    #[test]
    fn failed_commit_poison_is_not_guest_catchable() {
        let storage = Storage::default();
        let mut v = open(&storage, 100, 10);
        let id = v.create(DATA, "file", false).unwrap();
        v.write(id, None, b"old").unwrap();
        v.commit().unwrap();
        v.write(id, Some(0), b"new").unwrap();
        storage.fail_sync.store(true, Ordering::Relaxed);
        assert!(v.commit().is_err());
        storage.fail_sync.store(false, Ordering::Relaxed);
        assert!(v.stat(id).unwrap_err().downcast().is_err());
        assert!(v.create(DATA, "other", false).unwrap_err().downcast().is_err());
        assert!(v.commit().is_err());
        drop(v);
        let mut v = open(&storage, 100, 10);
        let bytes = contents(&mut v, id);
        assert!(bytes == b"old" || bytes == b"new");
        assert_eq!(v.usage(), (3, 1));
    }
}
