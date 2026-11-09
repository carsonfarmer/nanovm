//! The object's files: [`data`], its persistent volume, and [`tmp`], scratch
//! space that goes with its instance.
//!
//! Every change to `/data` is part of the request that makes it: it commits
//! with the request's answer or rolls back with its failure, all files
//! together. So there's no need for the write-to-a-temporary-then-rename
//! dance: a plain [`Dir::write`] is never seen torn or half done.
//!
//! `std::fs` doesn't work here (it uses WASI 0.2's filesystem, which nanovm
//! refuses); these use WASI 0.3's.

use wasip3::filesystem::preopens;
pub use wasip3::filesystem::types::{Descriptor, DescriptorFlags, ErrorCode, OpenFlags, PathFlags};
use wasip3::wit_stream;

thread_local! {
    // Fetched once per instance and leaked, so a `Dir` is `Copy` and can be
    // held across awaits.
    static MOUNTS: &'static [(Descriptor, String)] = Box::leak(preopens::get_directories().into_boxed_slice());
}

/// A mounted directory, `/data` or `/tmp`. Paths are relative to it and can't
/// leave it.
#[derive(Clone, Copy)]
pub struct Dir(&'static Descriptor);

/// `/data`: the object's own volume. It survives eviction and restarts, and
/// counts against the app's `disk_mb` and `max_files`.
pub fn data() -> Dir {
    Dir::at("/data").expect("nanovm mounts /data")
}

/// `/tmp`: fast scratch, not journaled, deleted with the instance. It shares
/// the disk budget with `/data`.
pub fn tmp() -> Dir {
    Dir::at("/tmp").expect("nanovm mounts /tmp")
}

impl Dir {
    /// The directory mounted at `path`.
    pub fn at(path: &str) -> Option<Dir> {
        MOUNTS.with(|m| m.iter().find(|(_, p)| p == path).map(|(d, _)| Dir(d)))
    }

    /// The directory's descriptor, for what this module doesn't wrap.
    pub fn descriptor(self) -> &'static Descriptor {
        self.0
    }

    pub async fn open(self, path: &str, open: OpenFlags, flags: DescriptorFlags) -> Result<Descriptor, ErrorCode> {
        self.0.open_at(PathFlags::empty(), path.into(), open, flags).await
    }

    /// `path`'s contents, or `None` if there is no such file.
    pub async fn read(self, path: &str) -> Result<Option<Vec<u8>>, ErrorCode> {
        let fd = match self.open(path, OpenFlags::empty(), DescriptorFlags::READ).await {
            Err(ErrorCode::NoEntry) => return Ok(None),
            fd => fd?,
        };
        let (rx, done) = fd.read_via_stream(0);
        let data = rx.collect().await;
        done.await?;
        Ok(Some(data))
    }

    /// [`Dir::read`], as text (invalid UTF-8 replaced).
    pub async fn read_text(self, path: &str) -> Result<Option<String>, ErrorCode> {
        Ok(self.read(path).await?.map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    /// Makes `data` the contents of `path`, creating it if needed.
    pub async fn write(self, path: &str, data: impl Into<Vec<u8>>) -> Result<(), ErrorCode> {
        let flags = DescriptorFlags::READ | DescriptorFlags::WRITE;
        let fd = self.open(path, OpenFlags::CREATE | OpenFlags::TRUNCATE, flags).await?;
        write_at(&fd, data.into(), 0).await
    }

    /// Adds `data` to the end of `path`, creating it if needed.
    pub async fn append(self, path: &str, data: impl Into<Vec<u8>>) -> Result<(), ErrorCode> {
        let fd = self.open(path, OpenFlags::CREATE, DescriptorFlags::READ | DescriptorFlags::WRITE).await?;
        let (mut tx, rx) = wit_stream::new();
        let done = fd.append_via_stream(rx);
        let data = data.into();
        wasip3::spawn_local(async move {
            let _ = tx.write_all(data).await;
        });
        done.await
    }

    /// Removes the file `path`.
    pub async fn remove(self, path: &str) -> Result<(), ErrorCode> {
        self.0.unlink_file_at(path.into()).await
    }

    /// Renames `from` to `to`, replacing any file at `to`.
    pub async fn rename(self, from: &str, to: &str) -> Result<(), ErrorCode> {
        self.0.rename_at(from.into(), self.0, to.into()).await
    }
}

/// Up to `len` bytes of `fd` from `offset`: fewer only at the end of the
/// file. With [`Dir::open`], reads a large file a piece at a time.
pub async fn read_at(fd: &Descriptor, offset: u64, len: usize) -> Result<Vec<u8>, ErrorCode> {
    use wit_bindgen::rt::async_support::StreamResult;
    let (mut rx, done) = fd.read_via_stream(offset);
    let mut data = Vec::with_capacity(len);
    while data.len() < len {
        let (status, buf) = rx.read(data).await;
        data = buf;
        if let StreamResult::Dropped = status {
            // The end of the file, or an error that `done` reports.
            drop(rx);
            done.await?;
            return Ok(data);
        }
    }
    Ok(data)
}

/// Writes `data` to `fd` at `offset`.
pub async fn write_at(fd: &Descriptor, data: Vec<u8>, offset: u64) -> Result<(), ErrorCode> {
    let (mut tx, rx) = wit_stream::new();
    let done = fd.write_via_stream(rx, offset);
    wasip3::spawn_local(async move {
        let _ = tx.write_all(data).await;
    });
    done.await
}

/// `e`'s name in WIT, such as `insufficient-space` (a full disk) or `quota`
/// (too many files).
pub fn code(e: &ErrorCode) -> String {
    // Debug prints `ErrorCode::InsufficientSpace`.
    let debug = format!("{e:?}");
    let mut out = String::new();
    for (i, c) in debug.trim_start_matches("ErrorCode::").chars().take_while(char::is_ascii_alphanumeric).enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('-');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}
