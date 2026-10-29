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
use wasmtime::component::{HasData, Linker, Resource};

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
/// Keys per `list-keys` page.
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

fn other(e: impl std::fmt::Debug) -> Error {
    Error::Other(format!("{e:?}"))
}

impl ObjState {
    fn key_path(&self, bucket: &Resource<Bucket>, key: &str) -> wasmtime::Result<Result<PathBuf, Error>> {
        let dir = &self.table.get(bucket)?.dir;
        Ok(match key.len() {
            1..=MAX_KEY => Ok(dir.join(B64.encode(key))),
            _ => Err(Error::Other(format!("keys are 1 to {MAX_KEY} bytes"))),
        })
    }

    fn get_value(&self, bucket: &Resource<Bucket>, key: &str) -> KvResult<Option<Vec<u8>>> {
        let path = match self.key_path(bucket, key)? {
            Ok(p) => p,
            Err(e) => return Ok(Err(e)),
        };
        Ok(match std::fs::read(path) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(other(e)),
        })
    }

    fn set_value(&self, bucket: &Resource<Bucket>, key: &str, value: &[u8]) -> KvResult<()> {
        let path = match self.key_path(bucket, key)? {
            Ok(p) => p,
            Err(e) => return Ok(Err(e)),
        };
        let old = std::fs::metadata(&path).ok().map(|m| m.len());
        if let Err(e) = self.quota.reserve(old, value.len() as u64) {
            return Ok(Err(other(e)));
        }
        let tmp = path.with_file_name(".tmp");
        let written = std::fs::write(&tmp, value).and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            self.quota.adjust(old.unwrap_or(0) as i64 - value.len() as i64, -i64::from(old.is_none()));
            return Ok(Err(other(e)));
        }
        Ok(Ok(()))
    }

    fn delete_value(&self, bucket: &Resource<Bucket>, key: &str) -> KvResult<()> {
        let path = match self.key_path(bucket, key)? {
            Ok(p) => p,
            Err(e) => return Ok(Err(e)),
        };
        if let Ok(md) = std::fs::metadata(&path) {
            if let Err(e) = std::fs::remove_file(&path) {
                return Ok(Err(other(e)));
            }
            self.quota.adjust(-(md.len() as i64), -1);
        }
        Ok(Ok(()))
    }
}

impl store::Host for &mut ObjState {
    /// Bucket `""` is `default`; others are `[A-Za-z0-9_-]{1,64}`.
    fn open(&mut self, identifier: String) -> KvResult<Resource<Bucket>> {
        let name = if identifier.is_empty() { "default" } else { &identifier };
        if name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return Ok(Err(Error::NoSuchStore));
        }
        let mut dir = self.volume.clone();
        for part in [".kv", name] {
            dir.push(part);
            if !dir.exists() {
                if let Err(e) = self.quota.check_new_entry() {
                    return Ok(Err(other(e)));
                }
                if let Err(e) = std::fs::create_dir(&dir) {
                    return Ok(Err(other(e)));
                }
                self.quota.adjust(0, 1);
            }
        }
        Ok(Ok(self.table.push(Bucket { dir })?))
    }
}

impl store::HostBucket for &mut ObjState {
    fn get(&mut self, bucket: Resource<Bucket>, key: String) -> KvResult<Option<Vec<u8>>> {
        self.get_value(&bucket, &key)
    }

    fn set(&mut self, bucket: Resource<Bucket>, key: String, value: Vec<u8>) -> KvResult<()> {
        self.set_value(&bucket, &key, &value)
    }

    fn delete(&mut self, bucket: Resource<Bucket>, key: String) -> KvResult<()> {
        self.delete_value(&bucket, &key)
    }

    fn exists(&mut self, bucket: Resource<Bucket>, key: String) -> KvResult<bool> {
        Ok(match self.key_path(&bucket, &key)? {
            Ok(p) => Ok(p.exists()),
            Err(e) => Err(e),
        })
    }

    /// Keys in sorted order, a page at a time; the cursor is the offset of
    /// the next page.
    fn list_keys(&mut self, bucket: Resource<Bucket>, cursor: Option<u64>) -> KvResult<KeyResponse> {
        let entries = match std::fs::read_dir(&self.table.get(&bucket)?.dir) {
            Ok(e) => e,
            Err(e) => return Ok(Err(other(e))),
        };
        let mut keys: Vec<String> = entries
            .flatten()
            .filter_map(|e| String::from_utf8(B64.decode(e.file_name().as_encoded_bytes()).ok()?).ok())
            .collect();
        keys.sort();
        let start = usize::try_from(cursor.unwrap_or(0)).unwrap_or(usize::MAX).min(keys.len());
        let end = (start + PAGE).min(keys.len());
        Ok(Ok(KeyResponse {
            cursor: (end < keys.len()).then_some(end as u64),
            keys: keys.drain(start..end).collect(),
        }))
    }

    fn drop(&mut self, bucket: Resource<Bucket>) -> wasmtime::Result<()> {
        self.table.delete(bucket)?;
        Ok(())
    }
}

impl atomics::Host for &mut ObjState {
    /// An absent key counts as 0; a value that isn't a decimal u64 is an
    /// error, as is overflow.
    fn increment(&mut self, bucket: Resource<Bucket>, key: String, delta: u64) -> KvResult<u64> {
        let current = match self.get_value(&bucket, &key)? {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        let n = match current.map(|v| String::from_utf8(v).ok()?.parse::<u64>().ok()) {
            None => 0,
            Some(Some(n)) => n,
            Some(None) => return Ok(Err(Error::Other("not a number".into()))),
        };
        let Some(n) = n.checked_add(delta) else {
            return Ok(Err(Error::Other("overflow".into())));
        };
        Ok(self.set_value(&bucket, &key, n.to_string().as_bytes())?.map(|()| n))
    }
}

impl batch::Host for &mut ObjState {
    fn get_many(
        &mut self,
        bucket: Resource<Bucket>,
        keys: Vec<String>,
    ) -> KvResult<Vec<Option<(String, Vec<u8>)>>> {
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            match self.get_value(&bucket, &key)? {
                Ok(v) => out.push(v.map(|v| (key, v))),
                Err(e) => return Ok(Err(e)),
            }
        }
        Ok(Ok(out))
    }

    fn set_many(&mut self, bucket: Resource<Bucket>, key_values: Vec<(String, Vec<u8>)>) -> KvResult<()> {
        for (key, value) in key_values {
            if let Err(e) = self.set_value(&bucket, &key, &value)? {
                return Ok(Err(e));
            }
        }
        Ok(Ok(()))
    }

    fn delete_many(&mut self, bucket: Resource<Bucket>, keys: Vec<String>) -> KvResult<()> {
        for key in keys {
            if let Err(e) = self.delete_value(&bucket, &key)? {
                return Ok(Err(e));
            }
        }
        Ok(Ok(()))
    }
}
