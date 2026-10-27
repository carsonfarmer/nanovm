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
    denials: u64,
    metrics: Arc<Metrics>,
    obj: Arc<ObjShared>,
}

impl QuotaGuard {
    /// A budget whose starting usage is what's already under `volume`, so a
    /// persistent volume starts each instance with its real usage.
    pub fn new(volume: &Path, limits: &Limits, metrics: Arc<Metrics>, obj: Arc<ObjShared>) -> std::io::Result<Self> {
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
        file.set_len(size).map_err(|_| ErrorCode::Io)?;
        q.bytes = (q.bytes + size).saturating_sub(before);
        Ok(())
    }

    fn check_new_entry(&self) -> Result<(), ErrorCode> {
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

    fn adjust(&self, bytes: i64, files: i64) {
        let mut q = self.0.lock();
        q.bytes = q.bytes.saturating_add_signed(bytes);
        q.files = q.files.saturating_add_signed(files);
    }
}

impl Inner {
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
        WasiFilesystem::create_directory_at(&stock(store), fd, path).await?;
        quota.adjust(0, 1);
        Ok(())
    }

    async fn remove_directory_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path: String,
    ) -> FilesystemResult<()> {
        WasiFilesystem::remove_directory_at(&stock(store), fd, path).await?;
        guard(store).adjust(0, -1);
        Ok(())
    }

    async fn open_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path_flags: PathFlags,
        path: String,
        open_flags: OpenFlags,
        flags: DescriptorFlags,
    ) -> FilesystemResult<Resource<Descriptor>> {
        let (inner, quota) = (stock(store), guard(store));
        let (mut created, mut freed) = (false, 0);
        if open_flags.intersects(OpenFlags::CREATE | OpenFlags::TRUNCATE) {
            match entry_size(&inner, &fd, &path).await {
                Some(size) if open_flags.contains(OpenFlags::TRUNCATE) => freed = size,
                Some(_) => {}
                None if open_flags.contains(OpenFlags::CREATE) => {
                    quota.check_new_entry()?;
                    created = true;
                }
                None => {}
            }
        }
        let new = WasiFilesystem::open_at(&inner, fd, path_flags, path, open_flags, flags).await?;
        quota.adjust(-(freed as i64), created as i64);
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
        // Renaming over an existing entry frees it, unless both paths name the
        // same file: POSIX makes that a no-op, and crediting it would let a
        // guest zero its usage by renaming a file onto itself.
        let id = async |fd: &Resource<Descriptor>, path: &str| {
            let h = WasiFilesystem::metadata_hash_at(&inner, dup(fd), PathFlags::empty(), path.to_string()).await;
            h.ok().map(|h| (h.lower, h.upper))
        };
        let replaced = match id(&new_fd, &new_path).await {
            Some(target) if id(&fd, &old_path).await != Some(target) => entry_size(&inner, &new_fd, &new_path).await,
            _ => None,
        };
        WasiFilesystem::rename_at(&inner, fd, old_path, new_fd, new_path).await?;
        if let Some(bytes) = replaced {
            quota.adjust(-(bytes as i64), -1);
        }
        Ok(())
    }

    async fn unlink_file_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path: String,
    ) -> FilesystemResult<()> {
        let (inner, quota) = (stock(store), guard(store));
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
