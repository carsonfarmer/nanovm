//! WASIp3 resources and bounded streams over the transactional inode store.
use std::{
    fs::File,
    path::Path,
    pin::Pin,
    sync::{Arc, atomic::Ordering::Relaxed},
    task::{Context, Poll},
};

use bytes::BytesMut;
use parking_lot::Mutex;
use tokio::sync::oneshot;
use wasmtime::{
    StoreContextMut,
    component::{
        Access, Accessor, Destination, FutureReader, HasData, Linker, Resource, Source, StreamConsumer, StreamProducer,
        StreamReader, StreamResult,
    },
};
use wasmtime_wasi::{
    filesystem::Descriptor,
    p3::{
        bindings::filesystem::{
            preopens,
            types::{
                self, Advice, DescriptorFlags, DescriptorStat, DescriptorType, DirectoryEntry, ErrorCode, Filesize,
                HostDescriptorWithStore, MetadataHashValue, NewTimestamp, OpenFlags, PathFlags,
            },
        },
        filesystem::{FilesystemError, FilesystemResult},
    },
};

use crate::{
    capped::CappedFile,
    config::Limits,
    metrics::Metrics,
    object::{ObjShared, ObjState},
    page_store::{DATA, TMP, Volume},
};

pub fn add_to_linker(linker: &mut Linker<ObjState>) -> wasmtime::Result<()> {
    wasmtime_wasi::p3::cli::add_to_linker(linker)?;
    wasmtime_wasi::p3::clocks::add_to_linker(linker)?;
    wasmtime_wasi::p3::random::add_to_linker(linker)?;
    types::add_to_linker::<_, QuotaFs>(linker, |s| QuotaView(s))?;
    preopens::add_to_linker::<_, QuotaFs>(linker, |s| QuotaView(s))?;
    Ok(())
}

#[derive(Clone)]
pub struct QuotaGuard(Arc<Mutex<Inner>>);

struct Inner {
    volume: Volume,
    metrics: Arc<Metrics>,
    obj: Arc<ObjShared>,
    denials: u64,
}

impl QuotaGuard {
    pub fn new(path: &Path, limits: &Limits, metrics: Arc<Metrics>, obj: Arc<ObjShared>) -> anyhow::Result<Self> {
        let file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.join("store.redb"))?;
        let storage = CappedFile::new(
            file,
            limits
                .storage_mb
                .checked_mul(1 << 20)
                .ok_or_else(|| anyhow::anyhow!("storage limit overflow"))?,
        )?;
        let volume = Volume::open(
            storage,
            limits
                .disk_mb
                .checked_mul(1 << 20)
                .ok_or_else(|| anyhow::anyhow!("disk limit overflow"))?,
            limits.max_files,
            65536,
        )?;
        // File sync alone does not durably publish the database's name.
        File::open(path)?.sync_all()?;
        Ok(Self(Arc::new(Mutex::new(Inner {
            volume,
            metrics,
            obj,
            denials: 0,
        }))))
    }

    pub fn commit(&self) -> std::io::Result<()> {
        self.0.lock().volume.commit().map_err(std::io::Error::other)
    }

    fn run<T>(&self, f: impl FnOnce(&mut Volume) -> FilesystemResult<T>) -> FilesystemResult<T> {
        let mut q = self.0.lock();
        let result = f(&mut q.volume);
        if let Err(e) = &result {
            if matches!(
                e.downcast_ref(),
                Some(ErrorCode::Quota | ErrorCode::InsufficientSpace | ErrorCode::NotPermitted)
            ) {
                q.metrics.fs_denied.fetch_add(1, Relaxed);
                q.denials += 1;
                if q.denials == 1 || q.denials.is_multiple_of(1000) {
                    q.metrics
                        .event("fs-denied", &q.obj.id, e.to_string(), &q.obj.context.lock());
                }
            }
        }
        result
    }
}

// The resource table and every active stream share one handle. The inode stays
// charged until the final reference, even if the guest drops its descriptor.
struct Handle {
    fs: QuotaGuard,
    id: u64,
    flags: DescriptorFlags,
}
impl Drop for Handle {
    fn drop(&mut self) {
        self.fs.0.lock().volume.release(self.id);
    }
}

fn get(s: &ObjState, fd: Resource<Descriptor>) -> wasmtime::Result<Arc<Handle>> {
    Ok(s.table.get(&Resource::<Arc<Handle>>::new_borrow(fd.rep()))?.clone())
}

fn push(s: &mut ObjState, fs: QuotaGuard, id: u64, flags: DescriptorFlags) -> wasmtime::Result<Resource<Descriptor>> {
    fs.0.lock().volume.retain(id);
    let h = Arc::new(Handle { fs, id, flags });
    let fd = s.table.push(h)?;
    Ok(Resource::new_own(fd.rep()))
}

pub struct QuotaFs;
pub struct QuotaView<'a>(&'a mut ObjState);
impl HasData for QuotaFs {
    type Data<'a> = QuotaView<'a>;
}
impl types::Host for QuotaView<'_> {
    fn convert_error_code(&mut self, error: FilesystemError) -> wasmtime::Result<ErrorCode> {
        error.downcast()
    }
}
impl types::HostDescriptor for QuotaView<'_> {
    fn drop(&mut self, fd: Resource<Descriptor>) -> wasmtime::Result<()> {
        self.0.table.delete(Resource::<Arc<Handle>>::new_own(fd.rep()))?;
        Ok(())
    }
}
impl preopens::Host for QuotaView<'_> {
    fn get_directories(&mut self) -> wasmtime::Result<Vec<(Resource<Descriptor>, String)>> {
        let fs = self.0.quota.clone();
        [(DATA, "/data"), (TMP, "/tmp")]
            .into_iter()
            .map(|(id, name)| {
                Ok((
                    push(
                        self.0,
                        fs.clone(),
                        id,
                        DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY,
                    )?,
                    name.into(),
                ))
            })
            .collect()
    }
}

fn on<T>(
    a: &Accessor<ObjState, QuotaFs>,
    fd: Resource<Descriptor>,
    f: impl FnOnce(&mut Volume, &Handle) -> FilesystemResult<T>,
) -> FilesystemResult<T> {
    a.with(|mut s| {
        let h = get(s.data_mut(), fd).map_err(FilesystemError::trap)?;
        h.fs.run(|v| f(v, &h))
    })
}

fn writable(h: &Handle) -> FilesystemResult<()> {
    if !h.flags.contains(DescriptorFlags::WRITE) {
        return Err(ErrorCode::BadDescriptor.into());
    }
    Ok(())
}

struct Reader {
    handle: Arc<Handle>,
    offset: u64,
    result: Option<oneshot::Sender<Result<(), ErrorCode>>>,
}
impl Drop for Reader {
    fn drop(&mut self) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(Ok(()));
        }
    }
}
impl StreamProducer<ObjState> for Reader {
    type Item = u8;
    type Buffer = BytesMut;
    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        store: StoreContextMut<'a, ObjState>,
        dst: Destination<'a, u8, BytesMut>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut dst = dst.as_direct(store, 65536);
        if dst.remaining().is_empty() {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let out = dst.remaining();
        let cap = out.len().min(65536);
        let result = self.handle.fs.run(|v| {
            if !self.handle.flags.contains(DescriptorFlags::READ) {
                return Err(ErrorCode::BadDescriptor.into());
            }
            v.read(self.handle.id, self.offset, &mut out[..cap])
        });
        let status = match result {
            Ok(0) => StreamResult::Dropped,
            Ok(n) => {
                self.offset += n as u64;
                dst.mark_written(n);
                StreamResult::Completed
            }
            Err(e) => {
                let code = e.downcast()?;
                if let Some(tx) = self.result.take() {
                    let _ = tx.send(Err(code));
                }
                StreamResult::Dropped
            }
        };
        Poll::Ready(Ok(status))
    }
}

struct Writer {
    handle: Arc<Handle>,
    offset: Option<u64>,
    result: Option<oneshot::Sender<Result<(), ErrorCode>>>,
}
impl Drop for Writer {
    fn drop(&mut self) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(Ok(()));
        }
    }
}
impl StreamConsumer<ObjState> for Writer {
    type Item = u8;
    fn poll_consume(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        store: StoreContextMut<ObjState>,
        src: Source<'_, u8>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut src = src.as_direct(store);
        let bytes = src.remaining();
        let result = self.handle.fs.run(|v| {
            writable(&self.handle)?;
            v.write(self.handle.id, self.offset, &bytes[..bytes.len().min(65536)])
        });
        match result {
            Ok(n) => {
                src.mark_read(n);
                if let Some(at) = &mut self.offset {
                    *at += n as u64;
                }
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Err(e) => {
                let code = e.downcast()?;
                if let Some(tx) = self.result.take() {
                    let _ = tx.send(Err(code));
                }
                Poll::Ready(Ok(StreamResult::Dropped))
            }
        }
    }
}

struct Directory {
    handle: Arc<Handle>,
    after: Option<String>,
    result: Option<oneshot::Sender<Result<(), ErrorCode>>>,
}
impl Drop for Directory {
    fn drop(&mut self) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(Ok(()));
        }
    }
}
impl StreamProducer<ObjState> for Directory {
    type Item = DirectoryEntry;
    type Buffer = Option<DirectoryEntry>;
    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, ObjState>,
        mut dst: Destination<'a, DirectoryEntry, Option<DirectoryEntry>>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if dst.remaining(&mut store) == Some(0) {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let result = self
            .handle
            .fs
            .run(|v| v.entries(self.handle.id, self.after.as_deref(), 1));
        match result {
            Ok(mut entries) => match entries.pop() {
                Some(entry) => {
                    self.after = Some(entry.name.clone());
                    dst.set_buffer(Some(entry));
                    Poll::Ready(Ok(StreamResult::Completed))
                }
                None => Poll::Ready(Ok(StreamResult::Dropped)),
            },
            Err(e) => {
                let code = e.downcast()?;
                if let Some(tx) = self.result.take() {
                    let _ = tx.send(Err(code));
                }
                Poll::Ready(Ok(StreamResult::Dropped))
            }
        }
    }
}

fn write_stream(
    mut store: Access<'_, ObjState, QuotaFs>,
    fd: Resource<Descriptor>,
    data: StreamReader<u8>,
    offset: Option<u64>,
) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
    let handle = get(store.data_mut(), fd)?;
    let (tx, rx) = oneshot::channel();
    data.pipe(
        &mut store,
        Writer {
            handle,
            offset,
            result: Some(tx),
        },
    )?;
    FutureReader::new(&mut store, rx)
}

impl HostDescriptorWithStore<ObjState> for QuotaFs {
    fn read_via_stream(
        mut store: Access<ObjState, Self>,
        fd: Resource<Descriptor>,
        offset: Filesize,
    ) -> wasmtime::Result<(StreamReader<u8>, FutureReader<Result<(), ErrorCode>>)> {
        let handle = get(store.data_mut(), fd)?;
        let (tx, rx) = oneshot::channel();
        Ok((
            StreamReader::new(
                &mut store,
                Reader {
                    handle,
                    offset,
                    result: Some(tx),
                },
            )?,
            FutureReader::new(&mut store, rx)?,
        ))
    }
    fn read_directory(
        mut store: Access<'_, ObjState, Self>,
        fd: Resource<Descriptor>,
    ) -> wasmtime::Result<(StreamReader<DirectoryEntry>, FutureReader<Result<(), ErrorCode>>)> {
        let handle = get(store.data_mut(), fd)?;
        let (tx, rx) = oneshot::channel();
        Ok((
            StreamReader::new(
                &mut store,
                Directory {
                    handle,
                    after: None,
                    result: Some(tx),
                },
            )?,
            FutureReader::new(&mut store, rx)?,
        ))
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
        a: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        _offset: Filesize,
        _length: Filesize,
        _advice: Advice,
    ) -> FilesystemResult<()> {
        on(a, fd, |v, h| {
            v.stat(h.id)?;
            Ok(())
        })
    }
    // Guest syncs cannot publish the host's request transaction.
    async fn sync_data(a: &Accessor<ObjState, Self>, fd: Resource<Descriptor>) -> FilesystemResult<()> {
        on(a, fd, |v, h| {
            v.stat(h.id)?;
            Ok(())
        })
    }
    async fn sync(a: &Accessor<ObjState, Self>, fd: Resource<Descriptor>) -> FilesystemResult<()> {
        Self::sync_data(a, fd).await
    }
    async fn get_flags(a: &Accessor<ObjState, Self>, fd: Resource<Descriptor>) -> FilesystemResult<DescriptorFlags> {
        on(a, fd, |_, h| Ok(h.flags))
    }
    async fn get_type(a: &Accessor<ObjState, Self>, fd: Resource<Descriptor>) -> FilesystemResult<DescriptorType> {
        on(a, fd, |v, h| Ok(v.stat(h.id)?.type_))
    }
    async fn stat(a: &Accessor<ObjState, Self>, fd: Resource<Descriptor>) -> FilesystemResult<DescriptorStat> {
        on(a, fd, |v, h| v.stat(h.id))
    }
    async fn stat_at(
        a: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        _flags: PathFlags,
        path: String,
    ) -> FilesystemResult<DescriptorStat> {
        on(a, fd, |v, h| {
            let id = v.resolve(h.id, &path)?;
            v.stat(id)
        })
    }
    async fn set_size(a: &Accessor<ObjState, Self>, fd: Resource<Descriptor>, size: Filesize) -> FilesystemResult<()> {
        on(a, fd, |v, h| {
            writable(h)?;
            v.resize(h.id, size)
        })
    }
    async fn set_times(
        a: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        atime: NewTimestamp,
        mtime: NewTimestamp,
    ) -> FilesystemResult<()> {
        on(a, fd, |v, h| v.set_times(h.id, atime, mtime))
    }
    async fn set_times_at(
        a: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        _flags: PathFlags,
        path: String,
        atime: NewTimestamp,
        mtime: NewTimestamp,
    ) -> FilesystemResult<()> {
        on(a, fd, |v, h| {
            let id = v.resolve(h.id, &path)?;
            v.set_times(id, atime, mtime)
        })
    }
    async fn create_directory_at(
        a: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path: String,
    ) -> FilesystemResult<()> {
        on(a, fd, |v, h| v.create(h.id, &path, true).map(|_| ()))
    }
    async fn remove_directory_at(
        a: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path: String,
    ) -> FilesystemResult<()> {
        on(a, fd, |v, h| v.unlink(h.id, &path, true))
    }
    async fn unlink_file_at(
        a: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path: String,
    ) -> FilesystemResult<()> {
        on(a, fd, |v, h| v.unlink(h.id, &path, false))
    }
    async fn readlink_at(
        a: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        path: String,
    ) -> FilesystemResult<String> {
        on(a, fd, |v, h| {
            v.resolve(h.id, &path)?;
            Err(ErrorCode::Invalid.into())
        })
    }
    async fn symlink_at(
        a: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        _old: String,
        _new: String,
    ) -> FilesystemResult<()> {
        on(a, fd, |_, _| Err(ErrorCode::NotPermitted.into()))
    }
    async fn link_at(
        a: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        _flags: PathFlags,
        _old: String,
        _new_fd: Resource<Descriptor>,
        _new: String,
    ) -> FilesystemResult<()> {
        on(a, fd, |_, _| Err(ErrorCode::NotPermitted.into()))
    }
    async fn rename_at(
        a: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        old: String,
        new_fd: Resource<Descriptor>,
        new: String,
    ) -> FilesystemResult<()> {
        let other = a
            .with(|mut s| get(s.data_mut(), new_fd))
            .map_err(FilesystemError::trap)?;
        on(a, fd, |v, h| v.rename(h.id, &old, other.id, &new))
    }
    async fn is_same_object(
        a: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        other: Resource<Descriptor>,
    ) -> wasmtime::Result<bool> {
        a.with(|mut s| Ok(get(s.data_mut(), fd)?.id == get(s.data_mut(), other)?.id))
    }
    async fn metadata_hash(
        a: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
    ) -> FilesystemResult<MetadataHashValue> {
        on(a, fd, |_, h| Ok(MetadataHashValue { lower: h.id, upper: 0 }))
    }
    async fn metadata_hash_at(
        a: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        _flags: PathFlags,
        path: String,
    ) -> FilesystemResult<MetadataHashValue> {
        on(a, fd, |v, h| {
            Ok(MetadataHashValue {
                lower: v.resolve(h.id, &path)?,
                upper: 0,
            })
        })
    }
    async fn open_at(
        a: &Accessor<ObjState, Self>,
        fd: Resource<Descriptor>,
        _path_flags: PathFlags,
        path: String,
        open: OpenFlags,
        mut flags: DescriptorFlags,
    ) -> FilesystemResult<Resource<Descriptor>> {
        a.with(|mut s| {
            let h = get(s.data_mut(), fd).map_err(FilesystemError::trap)?;
            let id = h.fs.run(|v| {
                if flags.intersects(
                    DescriptorFlags::FILE_INTEGRITY_SYNC
                        | DescriptorFlags::DATA_INTEGRITY_SYNC
                        | DescriptorFlags::REQUESTED_WRITE_SYNC,
                ) {
                    return Err(ErrorCode::Unsupported.into());
                }
                if open.contains(OpenFlags::DIRECTORY)
                    && open.intersects(OpenFlags::CREATE | OpenFlags::EXCLUSIVE | OpenFlags::TRUNCATE)
                {
                    return Err(ErrorCode::Invalid.into());
                }
                let id = match v.resolve(h.id, &path) {
                    Ok(_) if open.contains(OpenFlags::CREATE | OpenFlags::EXCLUSIVE) => {
                        return Err(ErrorCode::Exist.into());
                    }
                    Ok(id) => id,
                    Err(e)
                        if matches!(e.downcast_ref(), Some(ErrorCode::NoEntry)) && open.contains(OpenFlags::CREATE) =>
                    {
                        v.create(h.id, &path, false)?
                    }
                    Err(e) => return Err(e),
                };
                let dir = matches!(v.stat(id)?.type_, DescriptorType::Directory);
                if dir
                    && (flags.contains(DescriptorFlags::WRITE)
                        || open.intersects(OpenFlags::CREATE | OpenFlags::TRUNCATE))
                {
                    return Err(ErrorCode::IsDirectory.into());
                }
                if !dir && open.contains(OpenFlags::DIRECTORY) {
                    return Err(ErrorCode::NotDirectory.into());
                }
                if !flags.contains(DescriptorFlags::WRITE) {
                    flags |= DescriptorFlags::READ;
                }
                if dir {
                    flags |= DescriptorFlags::MUTATE_DIRECTORY;
                }
                if open.intersects(OpenFlags::CREATE | OpenFlags::TRUNCATE) {
                    flags |= DescriptorFlags::WRITE;
                }
                if open.contains(OpenFlags::TRUNCATE) {
                    v.resize(id, 0)?;
                }
                Ok(id)
            })?;
            push(s.data_mut(), h.fs.clone(), id, flags).map_err(FilesystemError::trap)
        })
    }
}
