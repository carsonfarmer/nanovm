//! The disk budget of an object's writable mounts (`/data` and `/tmp`).
//!
//! wasmtime-wasi enforces read-only vs read-write but has no notion of space,
//! so this module links its own `wasi:filesystem@0.3` host that wraps the
//! stock one. Every call is delegated, except that calls which can consume
//! bytes or directory entries are checked against the object's budget first,
//! and writes go through a counting stream consumer. Both writable mounts
//! share one budget; `/app` is read-only, so the stock host refuses every
//! mutation there before any usage changes. Hard links and symlinks are
//! refused so the accounting stays exact.

use std::os::unix::fs::FileExt;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
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

/// Disk budget and usage of one instance, shared with its in-flight write
/// streams.
#[derive(Clone)]
pub struct QuotaGuard(Arc<Mutex<Inner>>);

struct Inner {
    max_bytes: u64,
    max_files: u64,
    bytes: u64,
    files: u64,
    /// What the current request changed in the volume, to commit or roll
    /// back. Its log and removed files may each hold as much as the volume,
    /// so an object's disk use is at most three times its quota while a
    /// request runs.
    journal: Journal,
    denials: u64,
    metrics: Arc<Metrics>,
    obj: Arc<ObjShared>,
}

impl QuotaGuard {
    /// A budget whose starting usage is what's already under `volume`, so a
    /// persistent volume starts each instance with its real usage.
    pub fn new(volume: &Path, limits: &Limits, metrics: Arc<Metrics>, obj: Arc<ObjShared>) -> std::io::Result<Self> {
        // First, so a failed request or a crash is undone before it's counted.
        let journal = Journal::open(volume, limits.disk_mb << 20, obj.clone())?;
        let (mut bytes, mut files) = (0, 0);
        let mut stack = vec![volume.to_path_buf()];
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
        Ok(QuotaGuard(Arc::new(Mutex::new(Inner {
            max_bytes: limits.disk_mb << 20,
            max_files: limits.max_files,
            bytes,
            files,
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
        let allowed_end = end.min(room);
        if allowed_end <= at && !buf.is_empty() {
            return Err(q.full());
        }
        q.journaled(|j| j.writing(file, at, allowed_end))?;
        let n = file
            .write_at(&buf[..(allowed_end - at) as usize], at)
            .map_err(|_| ErrorCode::Io)?;
        q.bytes += before.max(at + n as u64) - before;
        Ok(n)
    }

    fn set_size(&self, file: &std::fs::File, size: u64) -> Result<(), ErrorCode> {
        let before = file.metadata().map_err(|_| ErrorCode::Io)?.len();
        let mut q = self.0.lock();
        let room = before + q.max_bytes.saturating_sub(q.bytes);
        if size > room {
            return Err(q.full());
        }
        q.journaled(|j| j.writing(file, size, before))?;
        file.set_len(size).map_err(|_| ErrorCode::Io)?;
        q.bytes = (q.bytes + size).saturating_sub(before);
        Ok(())
    }

    /// Makes room to replace a file of `old` bytes (`None`: a new file) with
    /// one of `new` bytes, or refuses as a full disk would.
    pub fn reserve(&self, old: Option<u64>, new: u64) -> Result<(), ErrorCode> {
        let mut q = self.0.lock();
        if old.is_none() && q.files >= q.max_files {
            let what = format!("entry limit reached ({} files)", q.max_files);
            return Err(q.deny(ErrorCode::Quota, what));
        }
        let freed = old.unwrap_or(0).min(q.bytes);
        if (q.bytes - freed).saturating_add(new) > q.max_bytes {
            return Err(q.full());
        }
        q.bytes = q.bytes - freed + new;
        q.files += u64::from(old.is_none());
        Ok(())
    }

    pub fn check_new_entry(&self) -> Result<(), ErrorCode> {
        let mut q = self.0.lock();
        if q.files >= q.max_files {
            let what = format!("entry limit reached ({} files)", q.max_files);
            return Err(q.deny(ErrorCode::Quota, what));
        }
        Ok(())
    }

    fn refuse(&self, code: ErrorCode, what: &str) -> FilesystemError {
        self.0.lock().deny(code, what.to_string()).into()
    }

    pub fn journal<T>(&self, f: impl FnOnce(&mut Journal) -> std::io::Result<T>) -> Result<T, ErrorCode> {
        self.0.lock().journaled(f)
    }

    /// Removes the volume's file at `path`, of `bytes`, into the journal:
    /// where it now is.
    pub fn remove(&self, path: &Path, bytes: u64) -> Result<std::path::PathBuf, ErrorCode> {
        let mut q = self.0.lock();
        let slot = q.journaled(|j| j.remove(path, bytes))?;
        q.bytes = q.bytes.saturating_sub(bytes);
        q.files = q.files.saturating_sub(1);
        Ok(slot)
    }

    /// Makes the current request's changes durable.
    pub fn commit(&self) -> std::io::Result<()> {
        self.0.lock().journal.commit()
    }

    pub fn adjust(&self, bytes: i64, files: i64) {
        let mut q = self.0.lock();
        q.bytes = q.bytes.saturating_add_signed(bytes);
        q.files = q.files.saturating_add_signed(files);
    }

    /// Bytes and entries in use.
    #[cfg(test)]
    pub(crate) fn usage(&self) -> (u64, u64) {
        let q = self.0.lock();
        (q.bytes, q.files)
    }
}

impl Inner {
    /// Runs `f` on the journal: a full journal is a full disk to the guest,
    /// and anything else an I/O error.
    fn journaled<T>(&mut self, f: impl FnOnce(&mut Journal) -> std::io::Result<T>) -> Result<T, ErrorCode> {
        f(&mut self.journal).map_err(|e| match e.kind() {
            std::io::ErrorKind::StorageFull => self.full(),
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
            self.metrics.event("fs-denied", &self.obj.app, &self.obj.id, what);
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
        let mut dirs = preopens::Host::get_directories(&mut self.0.filesystem())?;
        if let Some(app) = self.0.app_dir.clone() {
            dirs.push((
                self.0.filesystem().table.push(Descriptor::Dir(app))?,
                "/app".to_string(),
            ));
        }
        Ok(dirs)
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

impl Drop for QuotaWriter {
    fn drop(&mut self) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(Ok(()));
        }
    }
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
        sync_data() -> FilesystemResult<()>;
        sync() -> FilesystemResult<()>;
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
        if let Some(p) = local(store, &fd, &path) {
            quota.journal(|j| j.creating(&p))?;
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
        let local = local(store, &fd, &path);
        // In the volume, a truncation is made a `set_size` the journal sees.
        let truncate = local.is_some() && open_flags.contains(OpenFlags::TRUNCATE);
        let (mut created, mut freed) = (false, 0);
        if open_flags.intersects(OpenFlags::CREATE | OpenFlags::TRUNCATE) {
            match entry_size(&inner, &fd, &path).await {
                Some(size) if open_flags.contains(OpenFlags::TRUNCATE) && !truncate => freed = size,
                Some(_) => {}
                None if open_flags.contains(OpenFlags::CREATE) => {
                    quota.check_new_entry()?;
                    if let Some(p) = &local {
                        quota.journal(|j| j.creating(p))?;
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
                let src = std::fs::symlink_metadata(&from).map_err(|_| ErrorCode::NoEntry)?;
                let (mut file, mut dir) = (None, false);
                match std::fs::symlink_metadata(&to) {
                    Ok(d) if d.is_file() && src.is_dir() => return Err(ErrorCode::NotDirectory.into()),
                    Ok(d) if d.is_file() => file = Some((quota.remove(&to, d.len())?, d.len())),
                    Ok(_) => dir = quota.journal(|j| j.removing_dir(&to)).is_ok(),
                    Err(_) => {}
                }
                quota.journal(|j| j.renaming(&from, &to))?;
                if let Err(e) = WasiFilesystem::rename_at(&inner, fd, old_path, new_fd, new_path).await {
                    // Nothing was replaced after all.
                    if let Some((slot, bytes)) = file {
                        let _ = std::fs::rename(slot, &to);
                        quota.adjust(bytes as i64, 1);
                    }
                    return Err(e);
                }
                quota.adjust(0, -(dir as i64));
                Ok(())
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
            quota.remove(&p, md.len())?;
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
        let g = QuotaGuard::new(&dir, &limits, metrics.clone(), obj).unwrap();
        std::fs::remove_dir(dir).unwrap();
        g.0.lock().max_bytes = bytes;
        (g, metrics)
    }

    fn usage(g: &QuotaGuard) -> (u64, u64) {
        g.usage()
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
        let g = QuotaGuard::new(&dir, &limits, Default::default(), obj).unwrap();
        assert_eq!(usage(&g), (15, 4));
        assert_eq!(g.0.lock().max_bytes, 2 << 20);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A new file costs an entry and its bytes; replacing one costs only the
    /// difference, so a shrinking replacement frees space.
    #[test]
    fn reserve_charges_new_files_and_replacements() {
        let (g, _) = guard(100, 10);
        g.reserve(None, 60).unwrap();
        assert_eq!(usage(&g), (60, 1));
        g.reserve(Some(60), 100).unwrap();
        assert_eq!(usage(&g), (100, 1));
        g.reserve(Some(100), 0).unwrap();
        assert_eq!(usage(&g), (0, 1));
        // A replaced file can't free more than is in use.
        g.reserve(None, 5).unwrap();
        g.reserve(Some(1000), 1).unwrap();
        assert_eq!(usage(&g), (1, 2));
    }

    /// Past the byte limit a write is refused as a full disk would, leaving
    /// usage as it was; filling it exactly is allowed.
    #[test]
    fn reserve_refuses_past_the_byte_limit() {
        let (g, metrics) = guard(100, 10);
        g.reserve(None, 70).unwrap();
        assert!(matches!(g.reserve(None, 31), Err(ErrorCode::InsufficientSpace)));
        assert!(matches!(g.reserve(Some(70), 101), Err(ErrorCode::InsufficientSpace)));
        assert!(matches!(g.reserve(None, u64::MAX), Err(ErrorCode::InsufficientSpace)));
        assert_eq!(usage(&g), (70, 1));
        g.reserve(None, 30).unwrap();
        assert_eq!(usage(&g), (100, 2));
        assert_eq!(metrics.fs_denied.load(Relaxed), 3);
    }

    /// At the file limit no new entry is allowed, but existing files may
    /// still be replaced.
    #[test]
    fn reserve_and_new_entries_stop_at_the_file_limit() {
        let (g, _) = guard(100, 2);
        g.check_new_entry().unwrap();
        g.reserve(None, 1).unwrap();
        g.reserve(None, 1).unwrap();
        assert!(matches!(g.reserve(None, 0), Err(ErrorCode::Quota)));
        assert!(matches!(g.check_new_entry(), Err(ErrorCode::Quota)));
        g.reserve(Some(1), 50).unwrap();
        assert_eq!(usage(&g), (51, 2));
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
    #[test]
    fn denials_are_logged_sparingly() {
        let (g, metrics) = guard(0, 0);
        for _ in 0..2000 {
            let _ = g.check_new_entry();
        }
        assert_eq!(metrics.fs_denied.load(Relaxed), 2000);
        let events = metrics.events_since(0);
        assert_eq!(events.len(), 3);
        assert_eq!(
            (events[0].kind, &*events[0].app, &*events[0].object),
            ("fs-denied", "app", "obj")
        );
        assert!(events[0].detail.contains("entry limit reached (0 files)"));
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
}
