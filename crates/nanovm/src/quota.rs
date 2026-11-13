//! The disk budget of an object's writable mounts (`/data` and `/tmp`).
//!
//! wasmtime-wasi enforces read-only vs read-write but has no notion of space,
//! so this module links its own `wasi:filesystem@0.3` host that wraps the
//! stock one. Every call is delegated, except that calls which can consume
//! bytes or directory entries are checked against the object's budget first,
//! and writes go through a counting stream consumer. Both writable mounts
//! share one budget. Hard links and symlinks are refused so the accounting
//! stays exact.

use std::collections::HashMap;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::task::{Context, Poll};

use parking_lot::Mutex;
use tokio::sync::oneshot;
use wasmtime::component::{
    Access, Accessor, FutureReader, HasData, Linker, Resource, Source, StreamConsumer, StreamReader, StreamResult,
};
use wasmtime::{AsContextMut, StoreContextMut};
use wasmtime_wasi::filesystem::{Descriptor, WasiFilesystem, WasiFilesystemCtxView, WasiFilesystemView};
use wasmtime_wasi::p3::bindings::filesystem::preopens;
use wasmtime_wasi::p3::bindings::filesystem::types::{
    self, Advice, DescriptorFlags, DescriptorStat, DescriptorType, DirectoryEntry, ErrorCode, Filesize,
    HostDescriptorWithStore, MetadataHashValue, NewTimestamp, OpenFlags, PathFlags,
};
use wasmtime_wasi::p3::filesystem::{FilesystemError, FilesystemResult};
use wasmtime_wasi::{FsPerms, OpenMode};

use crate::config::Limits;
use crate::journal::Journal;
use crate::metrics::Metrics;
use crate::object::{ObjShared, ObjState};

/// Links WASIp3 cli, clocks, random and the quota'd filesystem. Sockets are
/// deliberately absent: the only network path is the egress-checked HTTP
/// client.
pub fn add_to_linker(linker: &mut Linker<ObjState>) -> wasmtime::Result<()> {
    wasmtime_wasi::p3::cli::add_to_linker(linker)?;
    wasmtime_wasi::p3::clocks::add_to_linker(linker)?;
    wasmtime_wasi::p3::random::add_to_linker(linker)?;
    types::add_to_linker::<_, QuotaFs>(linker, |s| QuotaView(s))?;
    preopens::add_to_linker::<_, QuotaFs>(linker, |s| QuotaView(s))?;
    Ok(())
}

/// What an app's objects hold on disk, against the app's budget, and the
/// free space its node keeps (`app_disk_mb`, `disk_reserve_mb`). Each
/// instance's [`QuotaGuard`] counts its changes here as well.
pub struct Disk {
    /// The bytes live instances count (`/tmp` too), and what the other
    /// objects' volumes held when last measured.
    used: AtomicU64,
    max: u64,
    /// The objects without an instance, and what each counts in `used`.
    idle: Mutex<HashMap<Arc<str>, u64>>,
    /// The filesystem the volumes are on, and the bytes it keeps free.
    fs: Option<std::fs::File>,
    pub reserve: u64,
}

impl Default for Disk {
    /// No budget and no reserve.
    fn default() -> Disk {
        Disk { used: AtomicU64::new(0), max: u64::MAX, idle: Mutex::default(), fs: None, reserve: 0 }
    }
}

impl Disk {
    /// The disk of the app whose objects are under `objects`, measured
    /// there; a budget of 0 MiB is a quarter of their filesystem, a reserve
    /// of 0 a twentieth, at most 1 GiB.
    pub fn open(objects: &Path, budget_mb: u64, reserve_mb: u64) -> std::io::Result<Disk> {
        let fs = std::fs::File::open(objects)?;
        let st = rustix::fs::fstatvfs(&fs)?;
        let mut idle = HashMap::new();
        // Each `<shard>/<id>/data` (see `app::object_dir`), not the pool.
        for shard in std::fs::read_dir(objects)?.filter(|e| e.as_ref().is_ok_and(|e| e.file_name().len() == 2)) {
            for object in std::fs::read_dir(shard?.path())? {
                let (object, volume) = object.map(|o| (o.file_name(), o.path().join(crate::journal::VOLUME)))?;
                if let Some(id) = object.to_str() && volume.is_dir() {
                    idle.insert(id.into(), usage(&volume)?.0);
                }
            }
        }
        Ok(Disk {
            used: AtomicU64::new(idle.values().sum()),
            max: if budget_mb == 0 { st.f_blocks * st.f_frsize / 4 } else { budget_mb << 20 },
            idle: Mutex::new(idle),
            fs: Some(fs),
            reserve: if reserve_mb == 0 { (st.f_blocks * st.f_frsize / 20).min(1 << 30) } else { reserve_mb << 20 },
        })
    }

    /// Why nothing more may be written, if the node's disk is below its
    /// reserve (or can't say how much it has free).
    fn low(&self) -> Option<String> {
        let free = rustix::fs::fstatvfs(self.fs.as_ref()?).map_or(0, |st| st.f_bavail * st.f_frsize);
        (free < self.reserve).then(|| format!("write refused, the node's disk is below its {:.1} MiB reserve", self.reserve as f64 / 1048576.0))
    }

    /// Up to `want` more bytes, none while the node's disk is below its
    /// reserve; and why not all of them.
    fn grant(&self, want: u64) -> (u64, String) {
        if want > 0 && let Some(why) = self.low() {
            return (0, why);
        }
        let mut got = 0;
        let _ = self.used.try_update(Relaxed, Relaxed, |u| {
            got = want.min(self.max.saturating_sub(u));
            Some(u + got)
        });
        (got, format!("write refused, the app's {} MiB disk budget is full", self.max >> 20))
    }

    fn add(&self, bytes: i128) {
        let _ = self.used.try_update(Relaxed, Relaxed, |u| Some((u as i128 + bytes).clamp(0, u64::MAX as i128) as u64));
    }

    /// `id`'s instance starts: its volume holds `bytes`.
    fn unpark(&self, id: &Arc<str>, bytes: u64) {
        let was = self.idle.lock().remove(id).unwrap_or(0);
        self.add(bytes as i128 - was as i128);
    }

    /// `id`'s instance, which counted `counted`, ends: its volume holds `bytes`.
    fn park(&self, id: &Arc<str>, counted: u64, bytes: u64) {
        let was = self.idle.lock().insert(id.clone(), bytes).unwrap_or(0);
        self.add(bytes as i128 - counted as i128 - was as i128);
    }
}

/// Bytes in the regular files under `dir`, and entries.
fn usage(dir: &Path) -> std::io::Result<(u64, u64)> {
    let (mut bytes, mut files) = (0, 0);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let (path, md) = entry.and_then(|e| Ok((e.path(), e.metadata()?)))?;
            files += 1;
            if md.is_dir() {
                stack.push(path);
            } else if md.is_file() {
                bytes += md.len();
            }
        }
    }
    Ok((bytes, files))
}

/// Disk budget and usage of one instance, shared with its in-flight write
/// streams.
#[derive(Clone)]
pub struct QuotaGuard(Arc<Mutex<Inner>>);

struct Inner {
    max_bytes: u64,
    max_files: u64,
    bytes: u64,
    files: u64,
    /// Its app's, which `bytes` counts in.
    disk: Arc<Disk>,
    /// What the current request changed in the volume, to commit or roll
    /// back. Its log and removed files may each hold as much as the volume,
    /// bounding logical content to three quotas. Filesystem block rounding
    /// and metadata are additional physical storage costs.
    journal: Journal,
    denials: u64,
    metrics: Arc<Metrics>,
    obj: Arc<ObjShared>,
    /// Last: it is dropped after the journal.
    park: Park,
}

impl QuotaGuard {
    /// A budget whose starting usage is what's already under the journal's
    /// volume, so a persistent volume starts each instance with its real
    /// usage, counted in `disk`, its app's.
    pub fn new(journal: Journal, limits: &Limits, metrics: Arc<Metrics>, obj: Arc<ObjShared>, disk: Arc<Disk>) -> std::io::Result<Self> {
        let (bytes, files) = usage(journal.volume())?;
        disk.unpark(&obj.id, bytes);
        Ok(QuotaGuard(Arc::new(Mutex::new(Inner {
            max_bytes: limits.disk_mb << 20,
            max_files: limits.max_files,
            bytes,
            files,
            park: Park { disk: disk.clone(), id: obj.id.clone(), counted: bytes, volume: journal.volume().into(), removed: 0 },
            disk,
            journal,
            denials: 0,
            metrics,
            obj,
        }))))
    }

    /// Writes as much of `buf` at `at` (or the end of the file) as the
    /// budget allows, so a filler sees a short write and then
    /// `insufficient-space`, as it would on a full disk.
    fn write(&self, file: &std::fs::File, at: Option<u64>, buf: &[u8]) -> Result<usize, ErrorCode> {
        let before = file.metadata().map_err(|_| ErrorCode::Io)?.len();
        let at = at.unwrap_or(before);
        let end = at.checked_add(buf.len() as u64).ok_or(ErrorCode::Overflow)?;
        let mut q = self.0.lock();
        let room = before + q.max_bytes.saturating_sub(q.bytes);
        let (got, short) = q.disk.grant(end.min(room).saturating_sub(before));
        let allowed_end = end.min(room).min(before + got);
        if allowed_end <= at && !buf.is_empty() {
            q.disk.add(-(got as i128));
            return Err(if end > room { q.full() } else { q.deny(ErrorCode::InsufficientSpace, short) });
        }
        let n = q.journaled(|j| j.writing(file, at, allowed_end)).and_then(|()| {
            file.write_at(&buf[..(allowed_end - at) as usize], at).map_err(|_| ErrorCode::Io)
        });
        let grown = n.as_ref().map_or(0, |n| before.max(at + *n as u64) - before);
        q.set_bytes(|b| b + grown, got);
        n
    }

    fn set_size(&self, file: &std::fs::File, size: u64) -> Result<(), ErrorCode> {
        let before = file.metadata().map_err(|_| ErrorCode::Io)?.len();
        let mut q = self.0.lock();
        let room = before + q.max_bytes.saturating_sub(q.bytes);
        if size > room {
            return Err(q.full());
        }
        let grown = size.saturating_sub(before);
        let (got, short) = q.disk.grant(grown);
        let done = match got < grown {
            true => Err(q.deny(ErrorCode::InsufficientSpace, short)),
            false => q.journaled(|j| j.writing(file, size, before)).and_then(|()| file.set_len(size).map_err(|_| ErrorCode::Io)),
        };
        if let Err(e) = done {
            q.disk.add(-(got as i128));
            return Err(e);
        }
        // A removed file's bytes were freed when it was removed; growing it
        // still costs, but shrinking it frees nothing more.
        let freed = if q.journal.holds(file) { 0 } else { before.saturating_sub(size) };
        q.set_bytes(|b| (b + grown).saturating_sub(freed), got);
        Ok(())
    }

    pub fn check_new_entry(&self) -> Result<(), ErrorCode> {
        let mut q = self.0.lock();
        if q.files >= q.max_files {
            let what = format!("entry limit reached ({} files)", q.max_files);
            return Err(q.deny(ErrorCode::Quota, what));
        }
        // An entry takes space too: none below the node's reserve.
        match q.disk.low() {
            Some(why) => Err(q.deny(ErrorCode::InsufficientSpace, why)),
            None => Ok(()),
        }
    }

    fn refuse(&self, code: ErrorCode, what: &str) -> FilesystemError {
        self.0.lock().deny(code, what.to_string()).into()
    }

    pub fn journal<T>(&self, f: impl FnOnce(&mut Journal) -> std::io::Result<T>) -> Result<T, ErrorCode> {
        self.0.lock().journaled(f)
    }

    pub fn unlink(&self, path: &Path, bytes: u64) -> Result<(), ErrorCode> {
        let mut q = self.0.lock();
        q.journaled(|j| j.unlink(path, bytes))?;
        q.set_bytes(|b| b.saturating_sub(bytes), 0);
        q.files = q.files.saturating_sub(1);
        Ok(())
    }

    /// Stage the target and undo before delegating the actual rename to WASI.
    async fn rename(
        &self,
        from: &Path,
        to: &Path,
        operation: impl std::future::Future<Output = FilesystemResult<()>>,
    ) -> FilesystemResult<()> {
        let src = std::fs::symlink_metadata(from).map_err(|_| ErrorCode::NoEntry)?;
        let replaced = std::fs::symlink_metadata(to).ok();
        if replaced.as_ref().is_some_and(|d| d.is_file()) && src.is_dir() {
            return Err(ErrorCode::NotDirectory.into());
        }
        let dir = replaced.as_ref().is_some_and(|d| !d.is_file());
        let (record, slot) = self.journal(|j| j.renaming(from, to, replaced.as_ref()))?;
        let file = slot.map(|slot| (slot, replaced.as_ref().map_or(0, |d| d.len())));
        if let Some((_, bytes)) = file {
            self.adjust(-(bytes as i64), -1);
        }
        if let Err(e) = operation.await {
            self.restore_rename(to, file, record)?;
            return Err(e);
        }
        self.journal(|j| {
            j.renamed(from, to);
            Ok(())
        })?;
        if let Some((slot, bytes)) = file {
            self.journal(|j| j.discard_created(&slot, bytes))?;
        }
        self.adjust(0, -(dir as i64));
        Ok(())
    }

    /// A failed rename restores its staged target and cancels its record.
    /// Restoration/cancellation errors poison the journal, so a guest cannot
    /// catch the error and commit the missing target.
    fn restore_rename(&self, to: &Path, file: Option<(std::path::PathBuf, u64)>, record: u64) -> Result<(), ErrorCode> {
        let mut q = self.0.lock();
        if let Some((slot, bytes)) = file {
            q.journal.restore(&slot, to, bytes).map_err(|_| ErrorCode::Io)?;
            q.set_bytes(|b| b + bytes, 0);
            q.files += 1;
        }
        q.journal.cancel_rename(record).map_err(|_| ErrorCode::Io)
    }

    /// Makes the current request's changes durable, on a thread of its own:
    /// a commit waits for the disk, which blocked the worker thread and every
    /// object it runs, so a node had no more commits in flight than threads.
    /// A request that changed nothing commits here: handing it over cost a
    /// read 130 µs of CPU. The thread, not this future, holds the volume
    /// until the commit ends: a request killed at its deadline drops this.
    pub async fn commit(&self) -> std::io::Result<()> {
        if !self.0.lock().journal.changed() {
            return self.0.lock().journal.commit();
        }
        let (q, obj) = (self.0.clone(), self.0.lock().obj.clone());
        let held = obj.storage().await;
        tokio::task::spawn_blocking(move || {
            let committed = q.lock().journal.commit();
            // Its last handle may end the instance's journal (see its
            // `Drop`), and counts its usage as its app's (see `Inner`'s),
            // which must both finish before the volume is free.
            drop((q, held));
            committed
        })
        .await?
    }

    pub fn adjust(&self, bytes: i64, files: i64) {
        let mut q = self.0.lock();
        q.set_bytes(|b| b.saturating_add_signed(bytes), 0);
        q.files = q.files.saturating_add_signed(files);
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let p = &mut self.park;
        (p.counted, p.volume, p.removed) = (self.bytes, self.journal.volume().to_path_buf(), self.journal.removed());
    }
}

/// An instance's usage, handed to its app when the instance ends: then
/// what the object's volume holds is the app's while it has no instance,
/// measured once the journal's own `Drop` has settled it (and rolled back
/// an object the instance made but never committed); what it held in
/// `/tmp` goes with it.
struct Park {
    disk: Arc<Disk>,
    id: Arc<str>,
    counted: u64,
    volume: std::path::PathBuf,
    /// What the instance removed and didn't commit.
    removed: u64,
}

impl Drop for Park {
    fn drop(&mut self) {
        let held = match usage(&self.volume) {
            // What it removed comes back when the object next starts.
            Ok((bytes, _)) => bytes + self.removed,
            // The object, emptied, is gone.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(_) => self.counted,
        };
        self.disk.park(&self.id, self.counted, held);
    }
}

impl Inner {
    /// Changes the object's bytes to `to` of them, and its app's by as
    /// much, less the `granted` bytes already counted there.
    fn set_bytes(&mut self, to: impl FnOnce(u64) -> u64, granted: u64) {
        let bytes = to(self.bytes);
        self.disk.add(bytes as i128 - self.bytes as i128 - granted as i128);
        self.bytes = bytes;
    }

    /// Runs `f` on the journal: a full journal is a full disk to the guest,
    /// and anything else an I/O error.
    fn journaled<T>(&mut self, f: impl FnOnce(&mut Journal) -> std::io::Result<T>) -> Result<T, ErrorCode> {
        self.journal
            .check()
            .and_then(|()| f(&mut self.journal))
            .map_err(|e| match e.kind() {
                // The node's disk, not the object's quota.
                std::io::ErrorKind::StorageFull if e.raw_os_error().is_some() => {
                    self.deny(ErrorCode::InsufficientSpace, "write refused, the node's disk is full".into())
                }
                std::io::ErrorKind::StorageFull => self.full(),
                // No room for another object (`Objects::admit`).
                std::io::ErrorKind::QuotaExceeded => self.deny(ErrorCode::InsufficientSpace, e.to_string()),
                // A path the journal refuses (`Journal::creating`), as the syscall would.
                std::io::ErrorKind::NotADirectory => ErrorCode::NotDirectory,
                std::io::ErrorKind::InvalidFilename => ErrorCode::NameTooLong,
                _ => ErrorCode::Io,
            })
    }

    fn full(&mut self) -> ErrorCode {
        let what = format!("write refused, {} MiB disk quota full", self.max_bytes >> 20);
        self.deny(ErrorCode::InsufficientSpace, what)
    }

    fn deny(&mut self, code: ErrorCode, what: String) -> ErrorCode {
        self.metrics.fs_denied.fetch_add(1, Relaxed);
        self.denials += 1;
        // A guest retrying in a loop shouldn't flood the event log.
        if self.denials == 1 || self.denials.is_multiple_of(1000) {
            self.metrics.event("fs-denied", &self.obj.id, what, &self.obj.context.lock());
        }
        code
    }
}

pub struct QuotaFs;

impl HasData for QuotaFs {
    type Data<'a> = QuotaView<'a>;
}

pub struct QuotaView<'a>(&'a mut ObjState);

fn fs(s: &mut ObjState) -> WasiFilesystemCtxView<'_> {
    s.filesystem()
}

fn stock(a: &Accessor<ObjState, QuotaFs>) -> Accessor<ObjState, WasiFilesystem> {
    a.with_getter::<WasiFilesystem>(fs)
}

fn stock_access<'a>(store: &'a mut Access<'_, ObjState, QuotaFs>) -> Access<'a, ObjState, WasiFilesystem> {
    Access::<ObjState, WasiFilesystem>::new(store.as_context_mut(), fs)
}

fn guard(a: &Accessor<ObjState, QuotaFs>) -> QuotaGuard {
    a.with(|mut s| s.data_mut().quota.clone())
}

/// A second handle to the same descriptor, for a lookup before delegating.
fn dup(fd: &Resource<Descriptor>) -> Resource<Descriptor> {
    Resource::new_borrow(fd.rep())
}

/// Bytes held by the entry at `path` (0 unless a regular file), or `None` if
/// nothing is there.
async fn entry_size(inner: &Accessor<ObjState, WasiFilesystem>, fd: &Resource<Descriptor>, path: &str) -> Option<u64> {
    let st = WasiFilesystem::stat_at(inner, dup(fd), PathFlags::empty(), path.to_string()).await;
    st.ok().map(|st| {
        if matches!(st.type_, DescriptorType::RegularFile) {
            st.size
        } else {
            0
        }
    })
}

/// Where `path` under the directory `fd` is, if in the object's volume.
fn local(a: &Accessor<ObjState, QuotaFs>, fd: &Resource<Descriptor>, path: &str) -> Option<std::path::PathBuf> {
    a.with(|mut s| {
        let s = s.data_mut();
        let Ok(Descriptor::Dir(d)) = s.filesystem().table.get(fd) else { return None };
        let dir = d.dir.clone();
        s.quota.0.lock().journal.local_at(&dir, path)
    })
}

/// The writable file behind `fd`, if any.
fn writable_file(s: &mut ObjState, fd: &Resource<Descriptor>) -> Option<(QuotaGuard, Arc<std::fs::File>)> {
    let quota = s.quota.clone();
    match s.filesystem().table.get(fd) {
        Ok(Descriptor::File(f)) if f.perms == FsPerms::ReadWrite && f.open_mode.contains(OpenMode::WRITE) => {
            Some((quota, f.file.clone()))
        }
        _ => None,
    }
}

impl types::Host for QuotaView<'_> {
    fn convert_error_code(&mut self, error: FilesystemError) -> wasmtime::Result<ErrorCode> {
        error.downcast()
    }
}

impl types::HostDescriptor for QuotaView<'_> {
    fn drop(&mut self, fd: Resource<Descriptor>) -> wasmtime::Result<()> {
        types::HostDescriptor::drop(&mut self.0.filesystem(), fd)
    }
}

impl preopens::Host for QuotaView<'_> {
    fn get_directories(&mut self) -> wasmtime::Result<Vec<(Resource<Descriptor>, String)>> {
        preopens::Host::get_directories(&mut self.0.filesystem())
    }
}

fn write_stream(
    mut store: Access<'_, ObjState, QuotaFs>,
    fd: Resource<Descriptor>,
    data: StreamReader<u8>,
    at: Option<u64>,
) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
    let Some((quota, file)) = writable_file(store.data_mut(), &fd) else {
        // Not writable: let the stock host produce the right error.
        return match at {
            Some(offset) => WasiFilesystem::write_via_stream(stock_access(&mut store), fd, data, offset),
            None => WasiFilesystem::append_via_stream(stock_access(&mut store), fd, data),
        };
    };
    let (tx, rx) = oneshot::channel();
    data.pipe(
        &mut store,
        QuotaWriter {
            file,
            at,
            quota,
            result: Some(tx),
        },
    )?;
    // A writer dropped without a failure to report reads as success.
    FutureReader::new(
        &mut store,
        async move { wasmtime::error::Ok(rx.await.unwrap_or(Ok(()))) },
    )
}

struct QuotaWriter {
    file: Arc<std::fs::File>,
    at: Option<u64>,
    quota: QuotaGuard,
    result: Option<oneshot::Sender<Result<(), ErrorCode>>>,
}

impl StreamConsumer<ObjState> for QuotaWriter {
    type Item = u8;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        store: StoreContextMut<ObjState>,
        src: Source<'_, u8>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let me = &mut *self;
        let mut src = src.as_direct(store);
        match me.quota.write(&me.file, me.at, src.remaining()) {
            Ok(n) => {
                src.mark_read(n);
                if let Some(at) = &mut me.at {
                    *at += n as u64;
                }
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Err(code) => {
                if let Some(tx) = me.result.take() {
                    let _ = tx.send(Err(code));
                }
                Poll::Ready(Ok(StreamResult::Dropped))
            }
        }
    }
}

/// Methods that need no quota check: forwarded to the stock implementation.
macro_rules! delegate {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {$(
        async fn $name(store: &Accessor<ObjState, Self>, fd: Resource<Descriptor>, $($arg: $ty),*) -> $ret {
            WasiFilesystem::$name(&stock(store), fd, $($arg),*).await
        }
    )*};
}

impl HostDescriptorWithStore<ObjState> for QuotaFs {
    delegate! {
        advise(offset: Filesize, length: Filesize, advice: Advice) -> FilesystemResult<()>;
        get_flags() -> FilesystemResult<DescriptorFlags>;
        get_type() -> FilesystemResult<DescriptorType>;
        stat() -> FilesystemResult<DescriptorStat>;
        stat_at(flags: PathFlags, path: String) -> FilesystemResult<DescriptorStat>;
        set_times(atime: NewTimestamp, mtime: NewTimestamp) -> FilesystemResult<()>;
        set_times_at(flags: PathFlags, path: String, atime: NewTimestamp, mtime: NewTimestamp) -> FilesystemResult<()>;
        readlink_at(path: String) -> FilesystemResult<String>;
        is_same_object(other: Resource<Descriptor>) -> wasmtime::Result<bool>;
        metadata_hash() -> FilesystemResult<MetadataHashValue>;
        metadata_hash_at(flags: PathFlags, path: String) -> FilesystemResult<MetadataHashValue>;
    }

    /// A guest's sync waits for nothing: a request's writes are durable
    /// once it commits, and rolled back if it fails, synced or not. SQLite
    /// syncs several times per transaction.
    async fn sync_data(store: &Accessor<ObjState, Self>, fd: Resource<Descriptor>) -> FilesystemResult<()> {
        WasiFilesystem::get_flags(&stock(store), fd).await.map(drop)
    }

    async fn sync(store: &Accessor<ObjState, Self>, fd: Resource<Descriptor>) -> FilesystemResult<()> {
        Self::sync_data(store, fd).await
    }

    fn read_via_stream(
        mut store: Access<ObjState, Self>,
        fd: Resource<Descriptor>,
        offset: Filesize,
    ) -> wasmtime::Result<(StreamReader<u8>, FutureReader<Result<(), ErrorCode>>)> {
        WasiFilesystem::read_via_stream(stock_access(&mut store), fd, offset)
    }

    fn read_directory(
        mut store: Access<'_, ObjState, Self>,
        fd: Resource<Descriptor>,
    ) -> wasmtime::Result<(StreamReader<DirectoryEntry>, FutureReader<Result<(), ErrorCode>>)> {
        WasiFilesystem::read_directory(stock_access(&mut store), fd)
    }

    fn write_via_stream(
        store: Access<'_, ObjState, Self>,
        fd: Resource<Descriptor>,
        data: StreamReader<u8>,
        offset: Filesize,
    ) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
        write_stream(store, fd, data, Some(offset))
    }

    fn append_via_stream(
        store: Access<'_, ObjState, Self>,
        fd: Resource<Descriptor>,
        data: StreamReader<u8>,
    ) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
        write_stream(store, fd, data, None)
    }

    async fn set_size(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        size: Filesize,
    ) -> FilesystemResult<()> {
        match store.with(|mut s| writable_file(s.data_mut(), &fd)) {
            Some((quota, file)) => Ok(quota.set_size(&file, size)?),
            None => WasiFilesystem::set_size(&stock(store), fd, size).await,
        }
    }

    async fn create_directory_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path: String,
    ) -> FilesystemResult<()> {
        let quota = guard(store);
        quota.check_new_entry()?;
        if let Some(mut p) = local(store, &fd, &path) {
            quota.journal(|j| {
                j.place(&mut p)?;
                j.creating(&p)
            })?;
        }
        WasiFilesystem::create_directory_at(&stock(store), fd, path).await?;
        quota.adjust(0, 1);
        Ok(())
    }

    async fn remove_directory_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path: String,
    ) -> FilesystemResult<()> {
        if let Some(p) = local(store, &fd, &path) {
            guard(store).journal(|j| j.removing_dir(&p))?;
        }
        WasiFilesystem::remove_directory_at(&stock(store), fd, path).await?;
        guard(store).adjust(0, -1);
        Ok(())
    }

    async fn open_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path_flags: PathFlags,
        path: String,
        mut open_flags: OpenFlags,
        flags: DescriptorFlags,
    ) -> FilesystemResult<Resource<Descriptor>> {
        let (inner, quota) = (stock(store), guard(store));
        let mut local = local(store, &fd, &path);
        // In the volume, a truncation is made a `set_size` the journal sees.
        let truncate = local.is_some() && open_flags.contains(OpenFlags::TRUNCATE);
        let (mut created, mut freed) = (false, 0);
        if open_flags.intersects(OpenFlags::CREATE | OpenFlags::TRUNCATE) {
            match entry_size(&inner, &fd, &path).await {
                Some(size) if open_flags.contains(OpenFlags::TRUNCATE) && !truncate => freed = size,
                Some(_) => {}
                None if open_flags.contains(OpenFlags::CREATE) => {
                    quota.check_new_entry()?;
                    if let Some(p) = &mut local {
                        quota.journal(|j| {
                            j.place(p)?;
                            j.creating(p)
                        })?;
                    }
                    created = true;
                }
                None => {}
            }
        }
        if truncate {
            open_flags &= !OpenFlags::TRUNCATE;
        }
        let new = WasiFilesystem::open_at(&inner, fd, path_flags, path, open_flags, flags).await?;
        quota.adjust(-(freed as i64), created as i64);
        if let Some(path) = local {
            // Register read-only opens too: any descriptor may keep an
            // unlinked new file alive, as may a stream cloned from it.
            let file = store.with(|mut s| match s.data_mut().filesystem().table.get(&new) {
                Ok(Descriptor::File(f)) => Some(f.file.clone()),
                _ => None,
            });
            if let Some(file) = file {
                quota.journal(|j| {
                    if created {
                        j.created(&path)?;
                    }
                    j.opened(&file)
                })?;
            }
        }
        if truncate && let Some((quota, file)) = store.with(|mut s| writable_file(s.data_mut(), &new)) {
            quota.set_size(&file, 0)?;
        }
        Ok(new)
    }

    async fn rename_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        old_path: String,
        new_fd: Resource<Descriptor>,
        new_path: String,
    ) -> FilesystemResult<()> {
        let (inner, quota) = (stock(store), guard(store));
        match (local(store, &fd, &old_path), local(store, &new_fd, &new_path)) {
            // POSIX makes renaming a file onto itself a no-op.
            (Some(from), Some(to)) if from == to => Ok(()),
            // The journal keeps what the rename replaces; the stock rename
            // makes it, with its own checks.
            (Some(from), Some(to)) => {
                quota
                    .rename(
                        &from,
                        &to,
                        WasiFilesystem::rename_at(&inner, fd, old_path, new_fd, new_path),
                    )
                    .await
            }
            // `/data` and `/tmp` are separate filesystems to the guest.
            (Some(_), None) | (None, Some(_)) => Err(ErrorCode::CrossDevice.into()),
            (None, None) => {
                // Renaming over an existing entry frees it, unless both paths
                // name the same file: POSIX makes that a no-op, and crediting
                // it would let a guest zero its usage by renaming a file onto
                // itself.
                let id = async |fd: &Resource<Descriptor>, path: &str| {
                    let h = WasiFilesystem::metadata_hash_at(&inner, dup(fd), PathFlags::empty(), path.to_string());
                    h.await.ok().map(|h| (h.lower, h.upper))
                };
                let replaced = match id(&new_fd, &new_path).await {
                    Some(target) if id(&fd, &old_path).await != Some(target) => {
                        entry_size(&inner, &new_fd, &new_path).await
                    }
                    _ => None,
                };
                WasiFilesystem::rename_at(&inner, fd, old_path, new_fd, new_path).await?;
                if let Some(bytes) = replaced {
                    quota.adjust(-(bytes as i64), -1);
                }
                Ok(())
            }
        }
    }

    async fn unlink_file_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path: String,
    ) -> FilesystemResult<()> {
        let (inner, quota) = (stock(store), guard(store));
        if let Some(p) = local(store, &fd, &path)
            && let Ok(md) = std::fs::symlink_metadata(&p)
            && md.is_file()
        {
            quota.unlink(&p, md.len())?;
            return Ok(());
        }
        let size = entry_size(&inner, &fd, &path).await.unwrap_or(0);
        WasiFilesystem::unlink_file_at(&inner, fd, path).await?;
        quota.adjust(-(size as i64), -1);
        Ok(())
    }

    async fn link_at(
        store: &Accessor<ObjState, Self>,
        _fd: Resource<Descriptor>,
        _old_flags: PathFlags,
        _old_path: String,
        _new_fd: Resource<Descriptor>,
        _new_path: String,
    ) -> FilesystemResult<()> {
        Err(guard(store).refuse(ErrorCode::NotPermitted, "hard links are not permitted"))
    }

    async fn symlink_at(
        store: &Accessor<ObjState, Self>,
        _fd: Resource<Descriptor>,
        _old_path: String,
        _new_path: String,
    ) -> FilesystemResult<()> {
        Err(guard(store).refuse(ErrorCode::NotPermitted, "symlinks are not permitted"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A new, empty directory under the system temp dir.
    fn tempdir(name: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, Relaxed);
        let dir = std::env::temp_dir().join(format!("nanovm-quota-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A guard over an empty volume with room for `bytes` and `files`.
    fn guard(bytes: u64, files: u64) -> (QuotaGuard, Arc<Metrics>) {
        let metrics = Arc::new(Metrics::default());
        let obj = Arc::new(ObjShared::new("app".into(), "obj"));
        let limits = Limits {
            max_files: files,
            ..Limits::default()
        };
        let dir = tempdir("empty");
        let g = QuotaGuard::new(crate::journal::Journal::open(&dir, limits.disk_mb << 20, obj.clone(), None, None).unwrap(), &limits, metrics.clone(), obj, Default::default()).unwrap();
        std::fs::remove_dir(dir).unwrap();
        g.0.lock().max_bytes = bytes;
        (g, metrics)
    }

    fn usage(g: &QuotaGuard) -> (u64, u64) {
        g.usage()
    }

    fn volume_guard(v: &Path, max: u64) -> QuotaGuard {
        let obj = Arc::new(ObjShared::new("app".into(), "obj"));
        QuotaGuard::new(Journal::open(v, max, obj.clone(), None, None).unwrap(), &Limits::default(), Default::default(), obj, Default::default()).unwrap()
    }

    #[tokio::test]
    async fn rename_log_refusal_restores_target_and_usage() {
        let base = tempdir("rename-log-full");
        let v = base.join("o");
        std::fs::create_dir(&v).unwrap();
        let from = v.join("s".repeat(200));
        let to = v.join("target");
        std::fs::write(&from, b"source").unwrap();
        std::fs::write(&to, b"target").unwrap();
        // Enough for the target's staging record, not the long source name.
        let g = volume_guard(&v, 180);
        assert!(
            g.rename(&from, &to, async { panic!("logging must refuse before the syscall") })
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&to).unwrap(), b"target");
        assert_eq!(std::fs::read(&from).unwrap(), b"source");
        assert_eq!(g.usage(), (12, 2));
        g.commit().await.unwrap();
        drop(g);
        let g = volume_guard(&v, 180);
        assert_eq!(g.usage(), (12, 2));
        assert_eq!(std::fs::read(&to).unwrap(), b"target");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn failed_renames_refund_retained_bytes_and_success_still_overwrites() {
        let base = tempdir("rename-syscall");
        let v = base.join("o");
        std::fs::create_dir(&v).unwrap();
        let (from, to) = (v.join("source"), v.join("target"));
        std::fs::write(&from, b"source").unwrap();
        std::fs::write(&to, vec![7; 600 << 10]).unwrap();
        let g = volume_guard(&v, 1 << 20);
        for _ in 0..10 {
            let result = g
                .rename(&from, &to, async {
                    // An actual failing syscall after preflight and logging;
                    // WASI can similarly reject a capability-relative path.
                    std::fs::rename(from.join("child"), &to).map_err(|_| ErrorCode::NotDirectory.into())
                })
                .await;
            assert!(result.is_err());
            assert_eq!(g.usage(), (6 + (600 << 10), 2));
            assert_eq!(std::fs::read(&to).unwrap(), vec![7; 600 << 10]);
            assert_eq!(std::fs::read(&from).unwrap(), b"source");
        }
        assert_eq!(g.0.lock().denials, 0, "failed attempts must not exhaust retained quota");
        g.rename(&from, &to, async {
            std::fs::rename(&from, &to).map_err(|_| ErrorCode::Io.into())
        })
        .await
        .unwrap();
        assert_eq!(g.usage(), (6, 1));
        assert_eq!(std::fs::read(&to).unwrap(), b"source");
        g.commit().await.unwrap();
        drop(g);
        assert_eq!(volume_guard(&v, 1 << 20).usage(), (6, 1));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn failed_rename_of_discarded_creation_cannot_steal_target_on_recovery() {
        let base = tempdir("rename-recovery");
        let v = base.join("o");
        std::fs::create_dir(&v).unwrap();
        let (from, to) = (v.join("new"), v.join("target"));
        std::fs::write(&to, b"original").unwrap();
        let g = volume_guard(&v, 4096);
        g.journal(|j| j.creating(&from)).unwrap();
        std::fs::write(&from, b"new").unwrap();
        g.journal(|j| j.created(&from)).unwrap();
        g.adjust(3, 1);
        assert!(
            g.rename(&from, &to, async { Err(ErrorCode::NotPermitted.into()) })
                .await
                .is_err()
        );
        g.unlink(&from, 3).unwrap();
        // Reuse and discard both paths, leaving the original target only
        // in a later slot. A stale failed rename would steal it on replay.
        g.journal(|j| j.creating(&from)).unwrap();
        std::fs::write(&from, b"reuse").unwrap();
        g.journal(|j| j.created(&from)).unwrap();
        g.adjust(5, 1);
        g.unlink(&from, 5).unwrap();
        g.unlink(&to, 8).unwrap();
        drop(g);
        let g = volume_guard(&v, 4096);
        assert_eq!(g.usage(), (8, 1));
        assert_eq!(std::fs::read(&to).unwrap(), b"original");
        assert!(!from.exists());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn restoration_failure_forbids_commit_and_keeps_recovery_data() {
        let base = tempdir("rename-restore-failure");
        let v = base.join("o");
        std::fs::create_dir(&v).unwrap();
        let (from, to) = (v.join("source"), v.join("target"));
        std::fs::write(&from, b"source").unwrap();
        std::fs::write(&to, b"target").unwrap();
        let g = volume_guard(&v, 4096);
        assert!(
            g.rename(&from, &to, async {
                // Simulate a host-side obstruction to restoring the staged file.
                std::fs::create_dir(&to).unwrap();
                Err(ErrorCode::NotPermitted.into())
            })
            .await
            .is_err()
        );
        assert!(g.commit().await.is_err());
        assert!(g.journal(|j| j.creating(&v.join("later"))).is_err());
        assert_eq!(g.usage(), (6, 1), "no credit for a failed restoration");
        std::fs::remove_dir(&to).unwrap(); // Repair the injected obstruction.
        drop(g);
        let g = volume_guard(&v, 4096);
        assert_eq!(g.usage(), (12, 2));
        assert_eq!(std::fs::read(&to).unwrap(), b"target");
        assert_eq!(std::fs::read(&from).unwrap(), b"source");
        std::fs::remove_dir_all(base).unwrap();
    }

    /// An app's objects share its budget: a write past it is cut short, then
    /// refused to every object of the app until one frees space. The budget
    /// counts every volume, not the journals or the pool, from the start; an
    /// instance counts what its volume holds, and when it ends hands back
    /// what it held elsewhere (`/tmp`), keeping what it removed uncommitted,
    /// and an object it made and never committed is measured once gone.
    #[test]
    fn objects_share_their_apps_budget() {
        use crate::app::{Objects, object_dir};
        let dir = tempdir("app-budget");
        let volume = |id: &str| object_dir(&dir, id).join(crate::journal::VOLUME);
        for (id, n) in [("a", 4), ("c", 6)] {
            std::fs::create_dir_all(volume(id)).unwrap();
            std::fs::write(volume(id).join("old"), vec![0; n]).unwrap();
        }
        std::fs::create_dir_all(object_dir(&dir, "a").join("shipped")).unwrap();
        std::fs::write(object_dir(&dir, "a").join("shipped/x"), [0; 100]).unwrap();
        let objects = Arc::new(Objects::load(dir.clone(), 100, &Limits::default(), 0).unwrap());
        std::fs::write(dir.join("~new").join("x"), [0; 100]).unwrap();
        let disk = Arc::new(Disk::open(&dir, 1, 0).unwrap());
        assert_eq!((disk.used.load(Relaxed), disk.max), (10, 1 << 20));
        let guard = |id: &str| {
            let obj = Arc::new(ObjShared::new("app".into(), id));
            let journal = Journal::open(&volume(id), 64 << 20, obj.clone(), None, Some(&objects)).unwrap();
            QuotaGuard::new(journal, &Limits::default(), Default::default(), obj, disk.clone()).unwrap()
        };
        let (a, b) = (guard("a"), guard("b"));
        assert_eq!(disk.used.load(Relaxed), 10);
        let scratch = tempdir("app-budget-scratch");
        let file = |name: &str| std::fs::File::options().read(true).write(true).create(true).truncate(true).open(scratch.join(name)).unwrap();
        let (fa, fb) = (file("a"), file("b"));
        assert_eq!(a.write(&fa, None, &vec![1; 1 << 20]).unwrap(), (1 << 20) - 10, "cut at the app's budget");
        assert!(matches!(b.write(&fb, None, b"x"), Err(ErrorCode::InsufficientSpace)));
        assert!(matches!(b.set_size(&fb, 1), Err(ErrorCode::InsufficientSpace)));
        assert_eq!(b.0.lock().denials, 2);
        a.set_size(&fa, 100).unwrap();
        assert_eq!(b.write(&fb, None, b"xy").unwrap(), 2);
        assert_eq!(disk.used.load(Relaxed), 4 + 6 + 100 + 2);
        // `a` ends: of its 104 bytes only its volume's 4 stay counted.
        drop(a);
        assert_eq!(disk.used.load(Relaxed), 4 + 6 + 2);
        std::fs::write(volume("a").join("old"), [0; 40]).unwrap();
        let a = guard("a");
        assert_eq!((disk.used.load(Relaxed), a.usage().0), (40 + 6 + 2, 40));
        // A removal it leaves uncommitted counts until the object next
        // starts, which rolls it back.
        a.unlink(&volume("a").join("old"), 40).unwrap();
        assert_eq!(disk.used.load(Relaxed), 6 + 2);
        drop(a);
        assert_eq!(disk.used.load(Relaxed), 40 + 6 + 2);
        assert_eq!(guard("a").usage().0, 40, "rolled back");
        // An object an instance made and never committed leaves nothing:
        // what it wrote is counted until then, measured after it is gone.
        let d = guard("d");
        let mut p = d.0.lock().journal.volume().join("f");
        d.journal(|j| j.place(&mut p).and_then(|()| j.creating(&p))).unwrap();
        let f = Arc::new(std::fs::File::options().create_new(true).read(true).write(true).open(&p).unwrap());
        d.journal(|j| j.created(&p).and_then(|()| j.opened(&f))).unwrap();
        assert_eq!(d.write(&f, Some(0), &[1; 30]).unwrap(), 30);
        assert_eq!(disk.used.load(Relaxed), 40 + 6 + 2 + 30);
        drop((d, f));
        assert_eq!(disk.used.load(Relaxed), 40 + 6 + 2, "rolled back and gone");
        assert!(!object_dir(&dir, "d").exists());
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(scratch).unwrap();
    }

    /// Below the node's reserve nothing grows and nothing new is made, but
    /// files are still overwritten in place and shrunk.
    #[test]
    fn below_the_reserve_only_growth_is_refused() {
        let dir = tempdir("reserve");
        let disk = Arc::new(Disk { reserve: u64::MAX, fs: Some(std::fs::File::open(&dir).unwrap()), ..Disk::default() });
        std::fs::create_dir(dir.join("v")).unwrap();
        let obj = Arc::new(ObjShared::new("app".into(), "v"));
        let journal = Journal::open(&dir.join("v"), 64 << 20, obj.clone(), None, None).unwrap();
        let g = QuotaGuard::new(journal, &Limits::default(), Default::default(), obj, disk).unwrap();
        let f = std::fs::File::options().read(true).write(true).create(true).truncate(true).open(dir.join("f")).unwrap();
        f.set_len(10).unwrap();
        assert_eq!(g.write(&f, Some(0), b"0123456789").unwrap(), 10, "in place");
        assert_eq!(g.write(&f, Some(5), b"0123456789").unwrap(), 5, "the rest would grow it");
        assert!(matches!(g.write(&f, None, b"x"), Err(ErrorCode::InsufficientSpace)));
        assert!(matches!(g.check_new_entry(), Err(ErrorCode::InsufficientSpace)));
        g.set_size(&f, 3).unwrap();
        assert!(matches!(g.set_size(&f, 4), Err(ErrorCode::InsufficientSpace)));
        assert_eq!(std::fs::read(dir.join("f")).unwrap(), b"012");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A new instance starts from what the volume already holds: every
    /// entry counts against the file limit, and regular files' bytes.
    #[test]
    fn starts_from_the_volumes_usage() {
        let dir = tempdir("usage");
        std::fs::write(dir.join("a"), [0; 10]).unwrap();
        std::fs::create_dir_all(dir.join("d/e")).unwrap();
        std::fs::write(dir.join("d/e/b"), [0; 5]).unwrap();
        let limits = Limits {
            disk_mb: 2,
            ..Limits::default()
        };
        let obj = Arc::new(ObjShared::new("app".into(), "obj"));
        let g = QuotaGuard::new(crate::journal::Journal::open(&dir, limits.disk_mb << 20, obj.clone(), None, None).unwrap(), &limits, Default::default(), obj, Default::default()).unwrap();
        assert_eq!(usage(&g), (15, 4));
        assert_eq!(g.0.lock().max_bytes, 2 << 20);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// At the file limit no new entry is allowed until one is removed.
    #[test]
    fn new_entries_stop_at_the_file_limit() {
        let (g, _) = guard(100, 2);
        g.check_new_entry().unwrap();
        g.adjust(2, 2);
        assert!(matches!(g.check_new_entry(), Err(ErrorCode::Quota)));
        g.adjust(0, -1);
        g.check_new_entry().unwrap();
    }

    /// Adjustments move usage both ways and never below zero.
    #[test]
    fn adjust_saturates() {
        let (g, _) = guard(100, 10);
        g.adjust(40, 3);
        g.adjust(-10, -1);
        assert_eq!(usage(&g), (30, 2));
        g.adjust(-1000, -1000);
        assert_eq!(usage(&g), (0, 0));
        for _ in 0..3 {
            g.adjust(i64::MAX, i64::MAX);
        }
        assert_eq!(usage(&g), (u64::MAX, u64::MAX));
    }

    /// A guest refused in a loop is logged on its first denial and every
    /// thousandth after, though every denial is counted.
    #[tokio::test]
    async fn denials_are_logged_sparingly() {
        use http_body_util::BodyExt;
        use opentelemetry_proto::tonic::{collector::logs::v1::ExportLogsServiceRequest, common::v1::any_value::Value};
        use prost::Message;
        let records = Arc::new(Mutex::new(Vec::new()));
        let output = records.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(crate::server::serve_http1(listener, move |req| {
            let output = output.clone();
            async move {
                let body = req.into_body().collect().await.unwrap().to_bytes();
                let logs = ExportLogsServiceRequest::decode(body).unwrap();
                output.lock().extend(logs.resource_logs.into_iter().flat_map(|r| r.scope_logs).flat_map(|s| s.log_records));
                crate::server::ok_json(serde_json::json!({}))
            }
        }));
        let telemetry = Arc::new(crate::telemetry::Telemetry::new(&endpoint, "app", "digest", None, &Default::default()).unwrap());
        let (g, metrics) = guard(0, 0);
        metrics.telemetry.set(Arc::downgrade(&telemetry)).unwrap();
        for _ in 0..2000 {
            let _ = g.check_new_entry();
        }
        assert_eq!(metrics.fs_denied.load(Relaxed), 2000);
        tokio::task::spawn_blocking(move || telemetry.flush()).await.unwrap().unwrap();
        let records = records.lock();
        assert_eq!(records.len(), 3);
        for record in &*records {
            for (key, value) in [("event.name", "fs-denied"), ("nanovm.object", "obj")] {
                assert!(record.attributes.iter().any(|a| a.key == key && a.value.as_ref().and_then(|v| v.value.as_ref()) == Some(&Value::StringValue(value.into()))));
            }
            assert!(matches!(record.body.as_ref().and_then(|v| v.value.as_ref()), Some(Value::StringValue(s)) if s.contains("entry limit reached (0 files)")));
        }
        server.abort();
    }

    /// Writes past the budget are cut short, then refused; overwriting
    /// bytes a file already has costs nothing, and an empty write always
    /// succeeds.
    #[test]
    fn writes_are_cut_at_the_budget() {
        let dir = tempdir("write");
        let (g, _) = guard(10, 10);
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.join("f"))
            .unwrap();
        assert_eq!(g.write(&f, None, b"123456").unwrap(), 6);
        assert_eq!(g.write(&f, None, b"abcdef").unwrap(), 4, "a short write");
        assert_eq!(usage(&g).0, 10);
        assert!(matches!(g.write(&f, None, b"x"), Err(ErrorCode::InsufficientSpace)));
        assert_eq!(g.write(&f, None, b"").unwrap(), 0);
        assert_eq!(
            g.write(&f, Some(0), b"ABCDEFGHIJ").unwrap(),
            10,
            "an overwrite in place"
        );
        assert_eq!(g.write(&f, Some(5), b"0123456789").unwrap(), 5);
        assert_eq!(std::fs::read(dir.join("f")).unwrap(), b"ABCDE01234");
        assert!(matches!(g.write(&f, Some(u64::MAX), b"x"), Err(ErrorCode::Overflow)));
        assert_eq!(usage(&g).0, 10);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A write past the end of a file is charged for the gap it leaves.
    #[test]
    fn sparse_writes_pay_for_the_hole() {
        let dir = tempdir("sparse");
        let (g, _) = guard(10, 10);
        let f = std::fs::File::create(dir.join("f")).unwrap();
        assert_eq!(g.write(&f, Some(8), b"abcd").unwrap(), 2);
        assert_eq!((usage(&g).0, f.metadata().unwrap().len()), (10, 10));
        assert!(matches!(g.write(&f, Some(20), b"x"), Err(ErrorCode::InsufficientSpace)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Growing a file is charged and refused past the budget; shrinking it
    /// frees the difference.
    #[test]
    fn set_size_is_charged() {
        let dir = tempdir("size");
        let (g, _) = guard(10, 10);
        let f = std::fs::File::create(dir.join("f")).unwrap();
        g.set_size(&f, 8).unwrap();
        assert_eq!(usage(&g).0, 8);
        assert!(matches!(g.set_size(&f, 11), Err(ErrorCode::InsufficientSpace)));
        g.set_size(&f, 10).unwrap();
        g.set_size(&f, 3).unwrap();
        assert_eq!((usage(&g).0, f.metadata().unwrap().len()), (3, 3));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A file removed while open had its bytes freed then; truncating it
    /// afterwards can't free them a second time.
    #[test]
    fn a_removed_file_is_freed_once() {
        let dir = tempdir("removed").canonicalize().unwrap();
        std::fs::write(dir.join("f"), [0; 10]).unwrap();
        std::fs::write(dir.join("g"), [0; 10]).unwrap();
        let obj = Arc::new(ObjShared::new("app".into(), "obj"));
        let g = QuotaGuard::new(crate::journal::Journal::open(&dir, Limits::default().disk_mb << 20, obj.clone(), None, None).unwrap(), &Limits::default(), Default::default(), obj, Default::default()).unwrap();
        let f = std::fs::File::options().write(true).open(dir.join("f")).unwrap();
        g.unlink(&dir.join("f"), 10).unwrap();
        assert_eq!(usage(&g), (10, 1));
        g.set_size(&f, 0).unwrap();
        assert_eq!(usage(&g).0, 10);
        g.set_size(&f, 4).unwrap();
        assert_eq!(usage(&g).0, 14, "growing it still costs");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Unlinked new files keep their data and removal-time storage charge
    /// until every independent open and stream clone has gone away.
    #[tokio::test]
    async fn new_unlinked_files_pay_until_the_last_handle_closes() {
        let dir = tempdir("new-unlinked").canonicalize().unwrap();
        let limits = Limits { disk_mb: 1, ..Limits::default() };
        let obj = Arc::new(ObjShared::new("app".into(), "obj"));
        let g = QuotaGuard::new(crate::journal::Journal::open(&dir, limits.disk_mb << 20, obj.clone(), None, None).unwrap(), &limits, Default::default(), obj, Default::default()).unwrap();
        let create = |name: &str| {
            let path = dir.join(name);
            g.journal(|j| j.creating(&path)).unwrap();
            let f = Arc::new(std::fs::File::options().create_new(true).read(true).write(true).open(&path).unwrap());
            g.journal(|j| { j.created(&path)?; j.opened(&f) }).unwrap();
            g.adjust(0, 1);
            g.write(&f, Some(0), &vec![7; 600 << 10]).unwrap();
            f
        };
        let a = create("a");
        let reader = Arc::new(std::fs::File::open(dir.join("a")).unwrap());
        g.journal(|j| j.opened(&reader)).unwrap();
        let stream = a.clone();
        g.unlink(&dir.join("a"), 600 << 10).unwrap();
        assert_eq!(usage(&g), (0, 0));
        let b = create("b");
        assert!(matches!(g.unlink(&dir.join("b"), 600 << 10), Err(ErrorCode::InsufficientSpace)));
        assert_eq!(usage(&g), (600 << 10, 1));
        g.set_size(&a, 1).unwrap();
        assert_eq!(usage(&g), (600 << 10, 1), "no second credit for truncation");
        g.write(&a, Some(0), b"old").unwrap();
        let mut data = [0; 3];
        reader.read_exact_at(&mut data, 0).unwrap();
        assert_eq!(&data, b"old");
        assert_eq!(usage(&g).0, (600 << 10) + 2, "growth is still charged");
        drop(a);
        drop(reader);
        assert!(matches!(g.unlink(&dir.join("b"), 600 << 10), Err(ErrorCode::InsufficientSpace)));
        // A commit cannot forgive storage still owned by a live stream.
        g.commit().await.unwrap();
        assert!(matches!(g.unlink(&dir.join("b"), 600 << 10), Err(ErrorCode::InsufficientSpace)));
        drop(stream);
        g.unlink(&dir.join("b"), 600 << 10).unwrap();
        assert_eq!(usage(&g), (2, 0), "post-removal growth keeps the existing conservative charge");
        assert_eq!(b.metadata().unwrap().len(), 600 << 10);
    }
}

#[cfg(test)]
impl QuotaGuard {
    /// Bytes and entries in use.
    pub(crate) fn usage(&self) -> (u64, u64) {
        let q = self.0.lock();
        (q.bytes, q.files)
    }
}
