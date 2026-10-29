//! `wasi:keyvalue` for objects, stored as files in the object's own volume:
//! bucket `b` is the directory `/data/.kv/b`, and each key one file in it,
//! named by the key in unpadded base64url so no key can name a path. Values
//! count against the object's disk budget and keys against its file limit,
//! like any other file, and are replaced atomically (written beside, then
//! renamed). An object serves one request at a time, so `increment` and the
//! batch calls need no locking.

use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use wasmtime::component::{HasData, Linker, Resource, ResourceTableError};

use crate::object::ObjState;

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/keyvalue",
        world: "wasi:keyvalue/imports",
        imports: { default: trappable },
        with: { "wasi:keyvalue/store.bucket": super::Bucket },
    });
}
use bindings::wasi::keyvalue::{atomics, batch, store};
use store::{Error, KeyResponse};

/// Longest key: its encoding must fit a file name (255 bytes).
const MAX_KEY: usize = 180;
/// Keys per `list-keys` page, and the most a batch call may name.
const PAGE: usize = 1000;

pub struct Bucket {
    dir: PathBuf,
}

type KvResult<T> = wasmtime::Result<Result<T, Error>>;

pub fn add_to_linker(linker: &mut Linker<ObjState>) -> wasmtime::Result<()> {
    store::add_to_linker::<_, Kv>(linker, |s| s)?;
    atomics::add_to_linker::<_, Kv>(linker, |s| s)?;
    batch::add_to_linker::<_, Kv>(linker, |s| s)?;
    Ok(())
}

struct Kv;
impl HasData for Kv {
    type Data<'a> = &'a mut ObjState;
}

/// Why a call failed: an error the guest sees, or a trap (a bad handle).
enum Fail {
    Guest(Error),
    Trap(wasmtime::Error),
}

impl<E: std::fmt::Debug> From<E> for Fail {
    fn from(e: E) -> Self {
        Fail::Guest(Error::Other(format!("{e:?}")))
    }
}

fn fail<T>(why: impl Into<String>) -> Result<T, Fail> {
    Err(Fail::Guest(Error::Other(why.into())))
}

fn trap(e: ResourceTableError) -> Fail {
    Fail::Trap(e.into())
}

/// Splits a result into what the guest sees and what traps.
fn done<T>(r: Result<T, Fail>) -> KvResult<T> {
    match r {
        Ok(v) => Ok(Ok(v)),
        Err(Fail::Guest(e)) => Ok(Err(e)),
        Err(Fail::Trap(e)) => Err(e),
    }
}

impl ObjState {
    fn key_path(&self, bucket: &Resource<Bucket>, key: &str) -> Result<PathBuf, Fail> {
        let dir = &self.table.get(bucket).map_err(trap)?.dir;
        match key.len() {
            1..=MAX_KEY => Ok(dir.join(B64.encode(key))),
            _ => fail(format!("keys are 1 to {MAX_KEY} bytes")),
        }
    }

    /// Reads a value if `budget` (bytes, shared across a batch, starting at
    /// the object's memory limit) has room: the host never holds more than
    /// the guest could.
    fn get_value(&self, bucket: &Resource<Bucket>, key: &str, budget: &mut u64) -> Result<Option<Vec<u8>>, Fail> {
        let path = self.key_path(bucket, key)?;
        let Ok(md) = std::fs::symlink_metadata(&path) else { return Ok(None) };
        *budget = match budget.checked_sub(md.len()) {
            Some(left) => left,
            None => return fail("more than the object's memory limit"),
        };
        Ok(Some(std::fs::read(path)?))
    }

    fn memory_budget(&self) -> u64 {
        self.limiter.max_bytes as u64
    }

    fn set_value(&self, bucket: &Resource<Bucket>, key: &str, value: &[u8]) -> Result<(), Fail> {
        let path = self.key_path(bucket, key)?;
        // The guest can also reach the bucket through its filesystem; a key
        // whose name it has taken with something else is refused up front.
        let old = match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_file() => Some(m.len()),
            Ok(_) => return fail("the key's file is not a regular file"),
            Err(_) => None,
        };
        self.quota.reserve(old, value.len() as u64)?;
        let tmp = path.with_file_name(".tmp");
        if let Err(e) = std::fs::write(&tmp, value).and_then(|()| std::fs::rename(&tmp, &path)) {
            let _ = std::fs::remove_file(&tmp);
            self.quota.adjust(old.unwrap_or(0) as i64 - value.len() as i64, -i64::from(old.is_none()));
            return Err(e.into());
        }
        Ok(())
    }

    fn delete_value(&self, bucket: &Resource<Bucket>, key: &str) -> Result<(), Fail> {
        let path = self.key_path(bucket, key)?;
        if let Ok(md) = std::fs::metadata(&path) {
            std::fs::remove_file(&path)?;
            self.quota.adjust(-(md.len() as i64), -1);
        }
        Ok(())
    }

    /// Bucket `""` is `default`; others are `[A-Za-z0-9_-]{1,64}`.
    fn open_bucket(&mut self, identifier: &str) -> Result<Resource<Bucket>, Fail> {
        let name = if identifier.is_empty() { "default" } else { identifier };
        if name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return Err(Fail::Guest(Error::NoSuchStore));
        }
        let mut dir = self.volume.clone();
        for part in [".kv", name] {
            dir.push(part);
            if !dir.exists() {
                self.quota.check_new_entry()?;
                std::fs::create_dir(&dir)?;
                self.quota.adjust(0, 1);
            }
        }
        self.table.push(Bucket { dir }).map_err(trap)
    }

    /// Keys in sorted order, a page at a time; the cursor is the offset of
    /// the next page. Only names that are key files count: a regular file,
    /// canonically encoded, of a valid length (the guest's filesystem can put
    /// anything here).
    fn list(&self, bucket: &Resource<Bucket>, cursor: Option<u64>) -> Result<KeyResponse, Fail> {
        let dir = &self.table.get(bucket).map_err(trap)?.dir;
        let mut keys: Vec<String> = std::fs::read_dir(dir)?
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter_map(|e| {
                let key = String::from_utf8(B64.decode(e.file_name().as_encoded_bytes()).ok()?).ok()?;
                let canonical = e.file_name().to_str() == Some(B64.encode(&key).as_str());
                (canonical && (1..=MAX_KEY).contains(&key.len())).then_some(key)
            })
            .collect();
        keys.sort();
        let start = usize::try_from(cursor.unwrap_or(0)).unwrap_or(usize::MAX).min(keys.len());
        let end = (start + PAGE).min(keys.len());
        Ok(KeyResponse {
            cursor: (end < keys.len()).then_some(end as u64),
            keys: keys.drain(start..end).collect(),
        })
    }

    /// An absent key counts as 0; a value that isn't a decimal u64 is an
    /// error, as is overflow.
    fn increment_value(&self, bucket: &Resource<Bucket>, key: &str, delta: u64) -> Result<u64, Fail> {
        let n = match self.get_value(bucket, key, &mut self.memory_budget())? {
            None => 0,
            Some(v) => match String::from_utf8(v).ok().and_then(|v| v.parse::<u64>().ok()) {
                Some(n) => n,
                None => return fail("not a number"),
            },
        };
        let Some(n) = n.checked_add(delta) else { return fail("overflow") };
        self.set_value(bucket, key, n.to_string().as_bytes())?;
        Ok(n)
    }
}

/// Host calls aren't interrupted by the CPU budget, so a batch is bounded.
fn check_batch(n: usize) -> Result<(), Fail> {
    if n > PAGE {
        return fail(format!("at most {PAGE} keys per batch"));
    }
    Ok(())
}

impl store::Host for &mut ObjState {
    fn open(&mut self, identifier: String) -> KvResult<Resource<Bucket>> {
        done(self.open_bucket(&identifier))
    }
}

impl store::HostBucket for &mut ObjState {
    fn get(&mut self, bucket: Resource<Bucket>, key: String) -> KvResult<Option<Vec<u8>>> {
        done(self.get_value(&bucket, &key, &mut self.memory_budget()))
    }

    fn set(&mut self, bucket: Resource<Bucket>, key: String, value: Vec<u8>) -> KvResult<()> {
        done(self.set_value(&bucket, &key, &value))
    }

    fn delete(&mut self, bucket: Resource<Bucket>, key: String) -> KvResult<()> {
        done(self.delete_value(&bucket, &key))
    }

    fn exists(&mut self, bucket: Resource<Bucket>, key: String) -> KvResult<bool> {
        done(self.key_path(&bucket, &key).map(|p| p.is_file()))
    }

    fn list_keys(&mut self, bucket: Resource<Bucket>, cursor: Option<u64>) -> KvResult<KeyResponse> {
        done(self.list(&bucket, cursor))
    }

    fn drop(&mut self, bucket: Resource<Bucket>) -> wasmtime::Result<()> {
        self.table.delete(bucket)?;
        Ok(())
    }
}

impl atomics::Host for &mut ObjState {
    fn increment(&mut self, bucket: Resource<Bucket>, key: String, delta: u64) -> KvResult<u64> {
        done(self.increment_value(&bucket, &key, delta))
    }
}

impl batch::Host for &mut ObjState {
    fn get_many(
        &mut self,
        bucket: Resource<Bucket>,
        keys: Vec<String>,
    ) -> KvResult<Vec<Option<(String, Vec<u8>)>>> {
        let mut budget = self.memory_budget();
        done(check_batch(keys.len()).and_then(|()| {
            keys.into_iter()
                .map(|k| Ok(self.get_value(&bucket, &k, &mut budget)?.map(|v| (k, v))))
                .collect()
        }))
    }

    fn set_many(&mut self, bucket: Resource<Bucket>, key_values: Vec<(String, Vec<u8>)>) -> KvResult<()> {
        done(check_batch(key_values.len()).and_then(|()| {
            key_values.iter().try_for_each(|(k, v)| self.set_value(&bucket, k, v))
        }))
    }

    fn delete_many(&mut self, bucket: Resource<Bucket>, keys: Vec<String>) -> KvResult<()> {
        done(check_batch(keys.len()).and_then(|()| keys.iter().try_for_each(|k| self.delete_value(&bucket, k))))
    }
}
