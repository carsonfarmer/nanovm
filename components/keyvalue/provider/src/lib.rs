//! Spin KV over the host's sole transactional filesystem.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use std::cell::Cell;
use util::wasip3::filesystem::types::{DescriptorFlags, DescriptorType, ErrorCode, OpenFlags};

mod bindings {
    wit_bindgen::generate!({ path: "../wit", world: "provider" });
}
use bindings::exports::spin::key_value::key_value::{Error, Guest, GuestStore, Store};
bindings::export!(Provider with_types_in bindings);
struct Provider;
pub struct Bucket {
    path: String,
    _slot: Slot,
}
// Bound open stores plus outstanding listings and each value.
const MAX_VALUE: usize = 1024 * 1024;
thread_local! {
    static LIVE: Cell<usize> = const { Cell::new(0) };
    static TEMP: Cell<u64> = const { Cell::new(0) };
}
struct Slot;
impl Slot {
    fn reserve() -> Result<Self, Error> {
        if LIVE.get() == 32 {
            return Err(Error::StoreTableFull);
        }
        LIVE.set(LIVE.get() + 1);
        Ok(Self)
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        LIVE.set(LIVE.get() - 1);
    }
}
fn error(e: ErrorCode) -> Error {
    Error::Other(format!("{e:?}"))
}
fn key_path(dir: &str, key: &str) -> Result<String, Error> {
    if !(1..=180).contains(&key.len()) {
        return Err(Error::Other("keys are 1 to 180 bytes".into()));
    }
    Ok(format!("{dir}/{}", B64.encode(key)))
}
impl Guest for Provider {
    type Store = Bucket;
}
impl GuestStore for Bucket {
    async fn open(label: String) -> Result<Store, Error> {
        // Labels name private directories, not shared deployment stores.
        if label.is_empty()
            || label.len() > 64
            || !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(Error::NoSuchStore);
        }
        let slot = Slot::reserve()?;
        let path = format!(".kv/{label}");
        Ok(Store::new(Bucket { path, _slot: slot }))
    }
    async fn get(&self, key: String) -> Result<Option<Vec<u8>>, Error> {
        let path = key_path(&self.path, &key)?;
        let fd = match util::open("/data", &path, OpenFlags::empty(), DescriptorFlags::READ).await {
            Ok(fd) => fd,
            Err(ErrorCode::NoEntry) => return Ok(None),
            Err(e) => return Err(error(e)),
        };
        let (mut stream, done) = fd.read_via_stream(0);
        let mut value = Vec::with_capacity(4096);
        loop {
            if value.len() == value.capacity() {
                value.reserve_exact(value.len().min(MAX_VALUE + 1 - value.len()));
            }
            let before = value.len();
            let (_, buffer) = stream.read(value).await;
            value = buffer;
            if value.len() > MAX_VALUE {
                return Err(Error::Other("value exceeds 1 MiB".into()));
            }
            if value.len() == before {
                break;
            }
        }
        drop(stream);
        done.await.map_err(error)?;
        Ok(Some(value))
    }
    async fn set(&self, key: String, value: Vec<u8>) -> Result<(), Error> {
        // Cancellation is deferred across the whole mutation, including error
        // cleanup. Traps/deadlines still roll back through the host journal.
        wit_bindgen::block_on(async {
            let path = key_path(&self.path, &key)?;
            if value.len() > MAX_VALUE {
                return Err(Error::Other("value exceeds 1 MiB".into()));
            }
            // Only writes create buckets; opening/reading one never spends quota.
            // Open first so an existing full store needs no free directory entry.
            match util::open("/data", &self.path, OpenFlags::DIRECTORY, DescriptorFlags::READ).await {
                Ok(_) => (),
                Err(ErrorCode::NoEntry) => {
                    let dir = util::mount("/data").ok_or(Error::AccessDenied)?;
                    for name in [".kv".to_owned(), self.path.clone()] {
                        match dir.create_directory_at(name).await {
                            Ok(()) | Err(ErrorCode::Exist) => (),
                            Err(e) => return Err(error(e)),
                        }
                    }
                }
                Err(e) => return Err(error(e)),
            }
            let mut attempts = 0;
            let (tmp, fd) = loop {
                let n = TEMP.get();
                TEMP.set(n.wrapping_add(1));
                let tmp = format!("{}/.spin-write-{n}", self.path);
                match util::open(
                    "/data",
                    &tmp,
                    OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
                    DescriptorFlags::WRITE,
                )
                .await
                {
                    Ok(fd) => break (tmp, fd),
                    Err(ErrorCode::Exist) if attempts < 32 => attempts += 1,
                    Err(e) => return Err(error(e)),
                }
            };
            let dir = util::mount("/data").ok_or(Error::AccessDenied)?;
            #[cfg(feature = "test-hooks")]
            {
                cut("created");
                if util::config::get("kv-cut").unwrap().as_deref() == Some("partial") {
                    util::write_all(&fd, value[..value.len() / 2].to_vec(), 0)
                        .await
                        .unwrap();
                    cut("partial");
                }
            }
            #[cfg(feature = "test-hooks")]
            if key == "cancel" && value.len() > 4096 {
                util::write_file("/data", "kv-paused", b"created".to_vec())
                    .await
                    .expect("signal");
                util::wasip3::clocks::monotonic_clock::wait_for(100_000_000).await;
            }
            let result = util::write_all(&fd, value, 0).await;
            drop(fd);
            #[cfg(feature = "test-hooks")]
            cut("written");
            let result = match result {
                Ok(()) => dir.rename_at(tmp.clone(), dir, path).await,
                Err(e) => Err(e),
            };
            #[cfg(feature = "test-hooks")]
            cut("renamed");
            if result.is_err() {
                // A failed operation may be caught; never commit its temporary data.
                dir.unlink_file_at(tmp)
                    .await
                    .expect("temporary cleanup failed; rollback required");
            }
            result.map_err(error)
        })
    }
    async fn delete(&self, key: String) -> Result<(), Error> {
        wit_bindgen::block_on(async {
            let path = key_path(&self.path, &key)?;
            match util::mount("/data")
                .ok_or(Error::AccessDenied)?
                .unlink_file_at(path)
                .await
            {
                Ok(()) | Err(ErrorCode::NoEntry) => Ok(()),
                Err(e) => Err(error(e)),
            }
        })
    }
    async fn exists(&self, key: String) -> Result<bool, Error> {
        let path = key_path(&self.path, &key)?;
        match util::open("/data", &path, OpenFlags::empty(), DescriptorFlags::READ).await {
            Ok(fd) => Ok(matches!(
                fd.get_type().await.map_err(error)?,
                DescriptorType::RegularFile
            )),
            Err(ErrorCode::NoEntry) => Ok(false),
            Err(e) => Err(error(e)),
        }
    }
    async fn get_keys(
        &self,
    ) -> (
        wit_bindgen::rt::async_support::StreamReader<String>,
        wit_bindgen::rt::async_support::FutureReader<Result<(), Error>>,
    ) {
        let (mut tx, rx) = bindings::wit_stream::new();
        let Ok(slot) = Slot::reserve() else {
            // The ABI's default completion needs no provider task. Outstanding
            // endpoints remain bounded by the host's component resource table.
            let (done_tx, done_rx) = bindings::wit_future::new(|| Err(Error::StoreTableFull));
            drop(done_tx);
            drop(tx);
            return (rx, done_rx);
        };
        let (done_tx, done_rx) = bindings::wit_future::new(|| Err(Error::Other("listing cancelled".into())));
        let path = self.path.clone();
        wit_bindgen::spawn_local(async move {
            let _slot = slot;
            let result = async {
                let dir = match util::open("/data", &path, OpenFlags::DIRECTORY, DescriptorFlags::READ).await {
                    Ok(dir) => dir,
                    Err(ErrorCode::NoEntry) => return Ok(()),
                    Err(e) => return Err(error(e)),
                };
                let (mut entries, done) = dir.read_directory();
                while let Some(entry) = entries.next().await {
                    if !matches!(entry.type_, DescriptorType::RegularFile) {
                        continue;
                    }
                    let Ok(bytes) = B64.decode(&entry.name) else {
                        continue;
                    };
                    let Ok(key) = String::from_utf8(bytes) else {
                        continue;
                    };
                    if !(1..=180).contains(&key.len()) {
                        continue;
                    }
                    // One entry in flight: dropping the consumer stops enumeration.
                    if tx.write_one(key).await.is_some() {
                        return Ok(());
                    }
                }
                drop(entries);
                done.await.map_err(error)
            }
            .await;
            drop(tx);
            let _ = done_tx.write(result).await;
        });
        (rx, done_rx)
    }
}

#[cfg(feature = "test-hooks")]
fn cut(stage: &str) {
    assert_ne!(
        util::config::get("kv-cut").unwrap().as_deref(),
        Some(stage),
        "KV recovery cut {stage}"
    );
}
