//! Disk quotas for an object's writable mounts (`/data` and `/tmp`).
//!
//! wasmtime-wasi enforces read-only vs read-write but has no notion of space,
//! so this module links its own `wasi:filesystem@0.3` host that wraps the
//! stock one. Every call is delegated, except that calls which can consume
//! bytes or directory entries on a quota'd mount are checked against that
//! mount's budget first, and writes go through a counting stream consumer.
//! Hard links, symlinks and renames across mounts are refused on quota'd
//! mounts so the accounting stays exact.

use std::collections::HashMap;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::task::{Context, Poll};

use parking_lot::Mutex;
use tokio::sync::oneshot;
use wasmtime::component::{
    Access, Accessor, FutureReader, HasData, Linker, Resource, Source, StreamConsumer, StreamReader,
    StreamResult,
};
use wasmtime::{AsContextMut, StoreContextMut};
use wasmtime_wasi::filesystem::{Descriptor, WasiFilesystem, WasiFilesystemCtxView, WasiFilesystemView};
use wasmtime_wasi::p3::bindings::filesystem::types::{
    self, Advice, DescriptorFlags, DescriptorStat, DescriptorType, DirectoryEntry, ErrorCode, Filesize,
    HostDescriptorWithStore, MetadataHashValue, NewTimestamp, OpenFlags, PathFlags,
};
use wasmtime_wasi::p3::bindings::filesystem::preopens;
use wasmtime_wasi::p3::filesystem::{FilesystemError, FilesystemResult};
use wasmtime_wasi::{FsPerms, OpenMode};

use crate::metrics::Metrics;
use crate::object::ObjState;

/// Links WASIp3 cli, clocks, random and the quota'd filesystem. Sockets are
/// deliberately absent: the only network path is the egress-checked HTTP
/// client.
pub fn add_to_linker(linker: &mut Linker<ObjState>) -> wasmtime::Result<()> {
    wasmtime_wasi::p3::cli::add_to_linker(linker)?;
    wasmtime_wasi::p3::clocks::add_to_linker(linker)?;
    wasmtime_wasi::p3::random::add_to_linker(linker)?;
    types::add_to_linker::<_, QuotaFs>(linker, view)?;
    preopens::add_to_linker::<_, QuotaFs>(linker, view)?;
    Ok(())
}

/// Budget and usage of one writable mount.
pub struct Quota {
    mount: &'static str,
    max_bytes: u64,
    max_files: u64,
    max_file_bytes: u64,
    bytes: u64,
    files: u64,
}

impl Quota {
    /// Measures what's already on disk under `root`, so a persistent volume
    /// starts each instance with its real usage.
    pub fn scan(
        mount: &'static str,
        root: &Path,
        max_bytes: u64,
        max_files: u64,
        max_file_bytes: u64,
    ) -> std::io::Result<Quota> {
        let mut q = Quota { mount, max_bytes, max_files, max_file_bytes, bytes: 0, files: 0 };
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let md = entry.metadata()?;
                q.files += 1;
                if md.is_dir() {
                    stack.push(entry.path());
                } else if md.is_file() {
                    q.bytes += md.len();
                }
            }
        }
        Ok(q)
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct Usage {
    pub mount: &'static str,
    pub bytes: u64,
    pub max_bytes: u64,
    pub files: u64,
    pub max_files: u64,
}

/// Quota state of one instance, shared with its in-flight write streams.
#[derive(Clone)]
pub struct QuotaGuard(Arc<Mutex<Inner>>);

struct Inner {
    mounts: Vec<Quota>,
    /// Which quota'd mount each open descriptor lives on, by resource rep.
    fds: HashMap<u32, usize>,
    denials: u64,
    metrics: Arc<Metrics>,
    app: Arc<str>,
    object: Arc<str>,
}

impl QuotaGuard {
    pub fn new(mounts: Vec<Quota>, metrics: Arc<Metrics>, app: Arc<str>, object: Arc<str>) -> Self {
        QuotaGuard(Arc::new(Mutex::new(Inner {
            mounts,
            fds: HashMap::new(),
            denials: 0,
            metrics,
            app,
            object,
        })))
    }

    pub fn usage(&self) -> Vec<Usage> {
        let q = self.0.lock();
        q.mounts
            .iter()
            .map(|m| Usage {
                mount: m.mount,
                bytes: m.bytes,
                max_bytes: m.max_bytes,
                files: m.files,
                max_files: m.max_files,
            })
            .collect()
    }

    fn mount_of(&self, fd: &Resource<Descriptor>) -> Option<usize> {
        self.0.lock().fds.get(&fd.rep()).copied()
    }

    /// Writes as much of `buf` at `at` (or the end of the file) as the
    /// mount's budget allows, so a filler sees a short write and then
    /// `insufficient-space`, as it would on a full disk.
    fn write(&self, m: usize, file: &std::fs::File, at: Option<u64>, buf: &[u8]) -> Result<usize, ErrorCode> {
        let before = file.metadata().map_err(|_| ErrorCode::Io)?.len();
        let at = at.unwrap_or(before);
        let end = at.checked_add(buf.len() as u64).ok_or(ErrorCode::Overflow)?;
        let mut q = self.0.lock();
        let quota = &q.mounts[m];
        let room = before + quota.max_bytes.saturating_sub(quota.bytes);
        let allowed_end = end.min(room).min(quota.max_file_bytes.max(before));
        if allowed_end <= at && !buf.is_empty() {
            return Err(if end > quota.max_file_bytes && quota.max_file_bytes <= room {
                let max = quota.max_file_bytes >> 20;
                q.deny(m, ErrorCode::FileTooLarge, format!("file would exceed max_file_mb ({max} MiB)"))
            } else {
                let max = quota.max_bytes >> 20;
                q.deny(m, ErrorCode::InsufficientSpace, format!("write refused, {max} MiB quota full"))
            });
        }
        let n = file.write_at(&buf[..(allowed_end - at) as usize], at).map_err(|_| ErrorCode::Io)?;
        let after = before.max(at + n as u64);
        q.mounts[m].bytes += after - before;
        Ok(n)
    }

    fn set_size(&self, m: usize, file: &std::fs::File, size: u64) -> Result<(), ErrorCode> {
        let before = file.metadata().map_err(|_| ErrorCode::Io)?.len();
        let mut q = self.0.lock();
        let quota = &q.mounts[m];
        if size > before {
            if size > quota.max_file_bytes {
                let max = quota.max_file_bytes >> 20;
                return Err(q.deny(m, ErrorCode::FileTooLarge, format!("set-size past max_file_mb ({max} MiB)")));
            }
            if quota.bytes + (size - before) > quota.max_bytes {
                let max = quota.max_bytes >> 20;
                return Err(q.deny(m, ErrorCode::InsufficientSpace, format!("set-size refused, {max} MiB quota")));
            }
        }
        file.set_len(size).map_err(|_| ErrorCode::Io)?;
        let quota = &mut q.mounts[m];
        quota.bytes = (quota.bytes + size).saturating_sub(before);
        Ok(())
    }

    fn check_new_entry(&self, m: usize) -> Result<(), ErrorCode> {
        let mut q = self.0.lock();
        let quota = &q.mounts[m];
        if quota.files >= quota.max_files {
            let max = quota.max_files;
            return Err(q.deny(m, ErrorCode::Quota, format!("entry limit reached ({max} files)")));
        }
        Ok(())
    }

    fn refuse(&self, m: usize, code: ErrorCode, what: &str) -> ErrorCode {
        self.0.lock().deny(m, code, what.to_string())
    }

    fn adjust(&self, m: usize, bytes: i64, files: i64) {
        let mut q = self.0.lock();
        let quota = &mut q.mounts[m];
        quota.bytes = quota.bytes.saturating_add_signed(bytes);
        quota.files = quota.files.saturating_add_signed(files);
    }

    fn track(&self, fd: &Resource<Descriptor>, m: usize) {
        self.0.lock().fds.insert(fd.rep(), m);
    }
}

impl Inner {
    fn deny(&mut self, m: usize, code: ErrorCode, what: String) -> ErrorCode {
        self.metrics.fs_denied.fetch_add(1, Relaxed);
        self.denials += 1;
        // A guest retrying in a loop shouldn't flood the event log.
        if self.denials == 1 || self.denials.is_multiple_of(1000) {
            let detail = format!("{}: {what}", self.mounts[m].mount);
            self.metrics.event("fs-denied", &self.app, &self.object, detail);
        }
        code
    }
}

pub struct QuotaFs;

impl HasData for QuotaFs {
    type Data<'a> = QuotaView<'a>;
}

pub struct QuotaView<'a>(&'a mut ObjState);

fn view(s: &mut ObjState) -> QuotaView<'_> {
    QuotaView(s)
}

fn fs(s: &mut ObjState) -> WasiFilesystemCtxView<'_> {
    s.filesystem()
}

fn stock(a: &Accessor<ObjState, QuotaFs>) -> Accessor<ObjState, WasiFilesystem> {
    a.with_getter::<WasiFilesystem>(fs)
}

fn guard(a: &Accessor<ObjState, QuotaFs>) -> QuotaGuard {
    a.with(|mut s| s.data_mut().quota.clone())
}

/// A second handle to the same descriptor, for a lookup before delegating.
fn dup(fd: &Resource<Descriptor>) -> Resource<Descriptor> {
    Resource::new_borrow(fd.rep())
}

fn no_entry(e: &FilesystemError) -> bool {
    matches!(e.downcast_ref(), Some(ErrorCode::NoEntry))
}

fn is_file(st: &DescriptorStat) -> bool {
    matches!(st.type_, DescriptorType::RegularFile)
}

/// The writable file behind `fd` if it lives on a quota'd mount.
fn quota_file(
    s: &mut ObjState,
    fd: &Resource<Descriptor>,
) -> Option<(QuotaGuard, usize, Arc<std::fs::File>)> {
    let quota = s.quota.clone();
    let m = quota.mount_of(fd)?;
    let view = s.filesystem();
    match view.table.get(fd) {
        Ok(Descriptor::File(f)) if f.perms == FsPerms::ReadWrite && f.open_mode.contains(OpenMode::WRITE) => {
            Some((quota, m, f.file.clone()))
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
        self.0.quota.0.lock().fds.remove(&fd.rep());
        types::HostDescriptor::drop(&mut self.0.filesystem(), fd)
    }
}

impl preopens::Host for QuotaView<'_> {
    fn get_directories(&mut self) -> wasmtime::Result<Vec<(Resource<Descriptor>, String)>> {
        let mut dirs = preopens::Host::get_directories(&mut self.0.filesystem())?;
        if let Some(app) = self.0.app_dir.clone() {
            dirs.push((self.0.filesystem().table.push(Descriptor::Dir(app))?, "/app".to_string()));
        }
        let mut q = self.0.quota.0.lock();
        for (fd, path) in &dirs {
            if let Some(m) = q.mounts.iter().position(|m| m.mount == path) {
                q.fds.insert(fd.rep(), m);
            }
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
    let Some((quota, mount, file)) = quota_file(store.data_mut(), &fd) else {
        let inner = Access::<ObjState, WasiFilesystem>::new(store.as_context_mut(), fs);
        return match at {
            Some(offset) => WasiFilesystem::write_via_stream(inner, fd, data, offset),
            None => WasiFilesystem::append_via_stream(inner, fd, data),
        };
    };
    let (tx, rx) = oneshot::channel();
    data.pipe(&mut store, QuotaWriter { file, at, mount, quota, result: Some(tx) })?;
    FutureReader::new(&mut store, async move { wasmtime::error::Ok(rx.await.unwrap_or(Ok(()))) })
}

struct QuotaWriter {
    file: Arc<std::fs::File>,
    at: Option<u64>,
    mount: usize,
    quota: QuotaGuard,
    result: Option<oneshot::Sender<Result<(), ErrorCode>>>,
}

impl QuotaWriter {
    fn close(&mut self, res: Result<(), ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(res);
        }
    }
}

impl Drop for QuotaWriter {
    fn drop(&mut self) {
        self.close(Ok(()));
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
        let res = me.quota.write(me.mount, &me.file, me.at, src.remaining());
        match res {
            Ok(n) => {
                src.mark_read(n);
                if let Some(at) = &mut me.at {
                    *at += n as u64;
                }
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Err(code) => {
                me.close(Err(code));
                Poll::Ready(Ok(StreamResult::Dropped))
            }
        }
    }
}

impl HostDescriptorWithStore<ObjState> for QuotaFs {
    fn read_via_stream(
        mut store: Access<ObjState, Self>,
        fd: Resource<Descriptor>,
        offset: Filesize,
    ) -> wasmtime::Result<(StreamReader<u8>, FutureReader<Result<(), ErrorCode>>)> {
        WasiFilesystem::read_via_stream(Access::<ObjState, WasiFilesystem>::new(store.as_context_mut(), fs), fd, offset)
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

    async fn advise(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        offset: Filesize,
        length: Filesize,
        advice: Advice,
    ) -> FilesystemResult<()> {
        WasiFilesystem::advise(&stock(store), fd, offset, length, advice).await
    }

    async fn sync_data(store: &Accessor<ObjState, Self>, fd: Resource<Descriptor>) -> FilesystemResult<()> {
        WasiFilesystem::sync_data(&stock(store), fd).await
    }

    async fn get_flags(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
    ) -> FilesystemResult<DescriptorFlags> {
        WasiFilesystem::get_flags(&stock(store), fd).await
    }

    async fn get_type(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
    ) -> FilesystemResult<DescriptorType> {
        WasiFilesystem::get_type(&stock(store), fd).await
    }

    async fn set_size(store: &Accessor<ObjState, Self>, fd: Resource<Descriptor>, size: Filesize) -> FilesystemResult<()> {
        match store.with(|mut s| quota_file(s.data_mut(), &fd)) {
            Some((quota, m, file)) => Ok(quota.set_size(m, &file, size)?),
            None => WasiFilesystem::set_size(&stock(store), fd, size).await,
        }
    }

    async fn set_times(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        data_access_timestamp: NewTimestamp,
        data_modification_timestamp: NewTimestamp,
    ) -> FilesystemResult<()> {
        WasiFilesystem::set_times(&stock(store), fd, data_access_timestamp, data_modification_timestamp).await
    }

    fn read_directory(
        mut store: Access<'_, ObjState, Self>,
        fd: Resource<Descriptor>,
    ) -> wasmtime::Result<(StreamReader<DirectoryEntry>, FutureReader<Result<(), ErrorCode>>)> {
        WasiFilesystem::read_directory(Access::<ObjState, WasiFilesystem>::new(store.as_context_mut(), fs), fd)
    }

    async fn sync(store: &Accessor<ObjState, Self>, fd: Resource<Descriptor>) -> FilesystemResult<()> {
        WasiFilesystem::sync(&stock(store), fd).await
    }

    async fn create_directory_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path: String,
    ) -> FilesystemResult<()> {
        let quota = guard(store);
        let m = quota.mount_of(&fd);
        if let Some(m) = m {
            quota.check_new_entry(m)?;
        }
        WasiFilesystem::create_directory_at(&stock(store), fd, path).await?;
        if let Some(m) = m {
            quota.adjust(m, 0, 1);
        }
        Ok(())
    }

    async fn stat(store: &Accessor<ObjState, Self>, fd: Resource<Descriptor>) -> FilesystemResult<DescriptorStat> {
        WasiFilesystem::stat(&stock(store), fd).await
    }

    async fn stat_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path_flags: PathFlags,
        path: String,
    ) -> FilesystemResult<DescriptorStat> {
        WasiFilesystem::stat_at(&stock(store), fd, path_flags, path).await
    }

    async fn set_times_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path_flags: PathFlags,
        path: String,
        data_access_timestamp: NewTimestamp,
        data_modification_timestamp: NewTimestamp,
    ) -> FilesystemResult<()> {
        WasiFilesystem::set_times_at(
            &stock(store),
            fd,
            path_flags,
            path,
            data_access_timestamp,
            data_modification_timestamp,
        )
        .await
    }

    async fn link_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        old_path_flags: PathFlags,
        old_path: String,
        new_fd: Resource<Descriptor>,
        new_path: String,
    ) -> FilesystemResult<()> {
        let quota = guard(store);
        if let Some(m) = quota.mount_of(&fd).or(quota.mount_of(&new_fd)) {
            return Err(quota.refuse(m, ErrorCode::NotPermitted, "hard links are not permitted").into());
        }
        WasiFilesystem::link_at(&stock(store), fd, old_path_flags, old_path, new_fd, new_path).await
    }

    async fn open_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path_flags: PathFlags,
        path: String,
        open_flags: OpenFlags,
        flags: DescriptorFlags,
    ) -> FilesystemResult<Resource<Descriptor>> {
        let inner = stock(store);
        let quota = guard(store);
        let Some(m) = quota.mount_of(&fd) else {
            return WasiFilesystem::open_at(&inner, fd, path_flags, path, open_flags, flags).await;
        };
        let (mut created, mut freed) = (false, 0);
        if open_flags.intersects(OpenFlags::CREATE | OpenFlags::TRUNCATE) {
            match WasiFilesystem::stat_at(&inner, dup(&fd), path_flags, path.clone()).await {
                Ok(st) if open_flags.contains(OpenFlags::TRUNCATE) && is_file(&st) => freed = st.size,
                Ok(_) => {}
                Err(e) if no_entry(&e) && open_flags.contains(OpenFlags::CREATE) => {
                    quota.check_new_entry(m)?;
                    created = true;
                }
                Err(_) => {}
            }
        }
        let new = WasiFilesystem::open_at(&inner, fd, path_flags, path, open_flags, flags).await?;
        quota.adjust(m, -(freed as i64), created as i64);
        quota.track(&new, m);
        Ok(new)
    }

    async fn readlink_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path: String,
    ) -> FilesystemResult<String> {
        WasiFilesystem::readlink_at(&stock(store), fd, path).await
    }

    async fn remove_directory_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path: String,
    ) -> FilesystemResult<()> {
        let quota = guard(store);
        let m = quota.mount_of(&fd);
        WasiFilesystem::remove_directory_at(&stock(store), fd, path).await?;
        if let Some(m) = m {
            quota.adjust(m, 0, -1);
        }
        Ok(())
    }

    async fn rename_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        old_path: String,
        new_fd: Resource<Descriptor>,
        new_path: String,
    ) -> FilesystemResult<()> {
        let inner = stock(store);
        let quota = guard(store);
        let (from, to) = (quota.mount_of(&fd), quota.mount_of(&new_fd));
        if from != to {
            let m = from.or(to).unwrap();
            return Err(quota.refuse(m, ErrorCode::CrossDevice, "rename across mounts").into());
        }
        let Some(m) = to else {
            return WasiFilesystem::rename_at(&inner, fd, old_path, new_fd, new_path).await;
        };
        // Renaming over an existing entry frees it, unless both paths name the
        // same file: POSIX makes that a no-op, and crediting it would let a
        // guest zero its usage by renaming a file onto itself.
        let old_id = WasiFilesystem::metadata_hash_at(&inner, dup(&fd), PathFlags::empty(), old_path.clone()).await;
        let new_id = WasiFilesystem::metadata_hash_at(&inner, dup(&new_fd), PathFlags::empty(), new_path.clone()).await;
        let same = matches!((&old_id, &new_id), (Ok(a), Ok(b)) if (a.lower, a.upper) == (b.lower, b.upper));
        let replaced = match same {
            true => None,
            false => WasiFilesystem::stat_at(&inner, dup(&new_fd), PathFlags::empty(), new_path.clone())
                .await
                .ok()
                .map(|st| if is_file(&st) { st.size } else { 0 }),
        };
        WasiFilesystem::rename_at(&inner, fd, old_path, new_fd, new_path).await?;
        if let Some(bytes) = replaced {
            quota.adjust(m, -(bytes as i64), -1);
        }
        Ok(())
    }

    async fn symlink_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        old_path: String,
        new_path: String,
    ) -> FilesystemResult<()> {
        let quota = guard(store);
        if let Some(m) = quota.mount_of(&fd) {
            return Err(quota.refuse(m, ErrorCode::NotPermitted, "symlinks are not permitted").into());
        }
        WasiFilesystem::symlink_at(&stock(store), fd, old_path, new_path).await
    }

    async fn unlink_file_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path: String,
    ) -> FilesystemResult<()> {
        let inner = stock(store);
        let quota = guard(store);
        let Some(m) = quota.mount_of(&fd) else {
            return WasiFilesystem::unlink_file_at(&inner, fd, path).await;
        };
        let size = WasiFilesystem::stat_at(&inner, dup(&fd), PathFlags::empty(), path.clone())
            .await
            .map(|st| if is_file(&st) { st.size } else { 0 })
            .unwrap_or(0);
        WasiFilesystem::unlink_file_at(&inner, fd, path).await?;
        quota.adjust(m, -(size as i64), -1);
        Ok(())
    }

    async fn is_same_object(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        other: Resource<Descriptor>,
    ) -> wasmtime::Result<bool> {
        WasiFilesystem::is_same_object(&stock(store), fd, other).await
    }

    async fn metadata_hash(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
    ) -> FilesystemResult<MetadataHashValue> {
        WasiFilesystem::metadata_hash(&stock(store), fd).await
    }

    async fn metadata_hash_at(
        store: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path_flags: PathFlags,
        path: String,
    ) -> FilesystemResult<MetadataHashValue> {
        WasiFilesystem::metadata_hash_at(&stock(store), fd, path_flags, path).await
    }
}
