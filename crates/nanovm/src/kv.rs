//! `wasi:keyvalue` for objects, stored as files in the object's own volume:
//! bucket `b` is the directory `/data/.kv/b`, and each key one file in it,
//! named by the key in unpadded base64url so no key can name a path. Values
//! count against the object's disk budget and keys against its file limit,
//! like any other file, and each request's writes commit with it (see
//! `journal`). An object serves one request at a time, so `increment` and
//! the batch calls need no locking.

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
        // The old value goes into the request's journal and the new one is
        // written in its place; the request's commit makes it durable. A
        // write that fails traps, so the request rolls back.
        self.quota.reserve(old, value.len() as u64)?;
        if let Some(old) = old {
            self.quota.journal(|j| j.remove(&path, old))?;
        }
        self.quota.journal(|j| j.creating(&path))?;
        std::fs::write(&path, value).map_err(|e| Fail::Trap(e.into()))
    }

    fn delete_value(&self, bucket: &Resource<Bucket>, key: &str) -> Result<(), Fail> {
        let path = self.key_path(bucket, key)?;
        if let Ok(md) = std::fs::symlink_metadata(&path)
            && md.is_file()
        {
            self.quota.remove(&path, md.len())?;
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
                self.quota.journal(|j| j.creating(&dir))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Limits;

    fn state(limits: Limits) -> ObjState {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let volume = std::env::temp_dir().join(format!("nanovm-kv-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&volume);
        ObjState::for_test(volume, &limits)
    }

    /// What the guest is told, for a call that fails without trapping.
    fn why<T>(r: Result<T, Fail>) -> String {
        match r {
            Ok(_) => panic!("succeeded"),
            Err(Fail::Guest(Error::Other(s))) => s,
            Err(Fail::Guest(e)) => format!("{e:?}"),
            Err(Fail::Trap(e)) => panic!("trapped: {e}"),
        }
    }

    fn ok<T>(r: Result<T, Fail>) -> T {
        match r {
            Ok(v) => v,
            Err(e) => panic!("{}", why::<()>(Err(e))),
        }
    }

    fn get(s: &ObjState, b: &Resource<Bucket>, key: &str) -> Option<Vec<u8>> {
        ok(s.get_value(b, key, &mut s.memory_budget()))
    }

    /// `""` is the default bucket; other names are 1 to 64 of
    /// `[A-Za-z0-9_-]`, so none is a path. Each directory made counts as an
    /// entry, once.
    #[test]
    fn buckets_are_named_safely() {
        let mut s = state(Limits::default());
        ok(s.open_bucket(""));
        assert!(s.volume.join(".kv/default").is_dir());
        assert_eq!(s.quota.usage(), (0, 2));
        ok(s.open_bucket("default"));
        ok(s.open_bucket(&"a".repeat(64)));
        ok(s.open_bucket("Mixed_case-1"));
        assert_eq!(s.quota.usage(), (0, 4));
        for bad in [".", "..", "a/b", "a.b", "é", " ", &"a".repeat(65)] {
            assert!(why(s.open_bucket(bad)).contains("NoSuchStore"), "{bad:?}");
        }
        assert_eq!(s.quota.usage(), (0, 4));
    }

    /// A bucket that would pass the file limit isn't made.
    #[test]
    fn buckets_count_against_the_file_limit() {
        let mut s = state(Limits {
            max_files: 1,
            ..Limits::default()
        });
        assert!(why(s.open_bucket("b")).contains("Quota"));
        assert!(s.volume.join(".kv").is_dir() && !s.volume.join(".kv/b").exists());
        assert_eq!(s.quota.usage(), (0, 1));
    }

    /// A key is 1 to 180 bytes (not characters) and names one file in the
    /// bucket's directory, whatever it contains.
    #[test]
    fn keys_are_encoded_and_bounded() {
        let mut s = state(Limits::default());
        let b = ok(s.open_bucket(""));
        let dir = s.volume.join(".kv/default");
        for key in ["k", "../../etc/passwd", "a/b\0c", &"é".repeat(90), &"x".repeat(MAX_KEY)] {
            let p = ok(s.key_path(&b, key));
            assert_eq!(p.parent(), Some(dir.as_path()), "{key:?}");
            assert_eq!(p.file_name().unwrap().to_str(), Some(B64.encode(key).as_str()));
            ok(s.set_value(&b, key, key.as_bytes()));
            assert_eq!(get(&s, &b, key).as_deref(), Some(key.as_bytes()));
        }
        for bad in ["", &"é".repeat(91), &"x".repeat(MAX_KEY + 1)] {
            assert_eq!(why(s.key_path(&b, bad)), "keys are 1 to 180 bytes");
        }
    }

    /// Values replace atomically and are charged like files: a new key costs
    /// an entry and its bytes, a replacement the difference, a delete frees
    /// both, and deleting nothing is fine.
    #[test]
    fn values_round_trip_and_are_charged() {
        let mut s = state(Limits::default());
        let b = ok(s.open_bucket(""));
        assert_eq!(get(&s, &b, "k"), None);
        ok(s.set_value(&b, "k", b"hello"));
        assert_eq!(s.quota.usage(), (5, 3));
        ok(s.set_value(&b, "k", b"hi"));
        assert_eq!((get(&s, &b, "k").unwrap(), s.quota.usage()), (b"hi".to_vec(), (2, 3)));
        ok(s.set_value(&b, "e", b""));
        assert_eq!((get(&s, &b, "e").unwrap(), s.quota.usage()), (vec![], (2, 4)));
        ok(s.delete_value(&b, "k"));
        ok(s.delete_value(&b, "k"));
        assert_eq!((get(&s, &b, "k"), s.quota.usage()), (None, (0, 3)));
        let names: Vec<_> = std::fs::read_dir(s.volume.join(".kv/default"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, [B64.encode("e")], "no temporary file is left");
    }

    /// A value past the disk budget, or a new key past the file limit, is
    /// refused and leaves nothing behind; replacing a key still works.
    #[test]
    fn values_respect_the_quota() {
        let mut s = state(Limits {
            disk_mb: 1,
            max_files: 3,
            ..Limits::default()
        });
        let b = ok(s.open_bucket(""));
        assert!(why(s.set_value(&b, "big", &vec![0; (1 << 20) + 1])).contains("InsufficientSpace"));
        assert_eq!(get(&s, &b, "big"), None);
        ok(s.set_value(&b, "a", &vec![0; 1 << 20]));
        // A replacement refused for space leaves the value it would replace.
        assert!(why(s.set_value(&b, "a", &vec![1; (1 << 20) + 1])).contains("InsufficientSpace"));
        assert_eq!(get(&s, &b, "a"), Some(vec![0; 1 << 20]));
        assert!(why(s.set_value(&b, "b", b"")).contains("Quota"));
        ok(s.set_value(&b, "a", b"small"));
        assert_eq!(s.quota.usage(), (5, 3));
    }

    /// A key whose file the guest replaced with something else through its
    /// filesystem is refused, not followed or clobbered.
    #[test]
    fn keys_taken_by_a_directory_are_refused() {
        let mut s = state(Limits::default());
        let b = ok(s.open_bucket(""));
        std::fs::create_dir(ok(s.key_path(&b, "d"))).unwrap();
        assert_eq!(why(s.set_value(&b, "d", b"x")), "the key's file is not a regular file");
        assert!(s.get_value(&b, "d", &mut s.memory_budget()).is_err());
        assert!(ok(s.list(&b, None)).keys.is_empty());
    }

    /// The staging file is the host's: a `.tmp` the guest made in a bucket
    /// through its filesystem (and was charged for) is not silently
    /// replaced with its charge kept.
    #[test]
    fn guest_tmp_files_are_not_leaked() {
        let mut s = state(Limits::default());
        let b = ok(s.open_bucket(""));
        // What the guest's own `open_at` and write would have charged.
        std::fs::write(s.volume.join(".kv/default/.tmp"), [0; 100]).unwrap();
        s.quota.adjust(100, 1);
        ok(s.set_value(&b, "k", b"v"));
        let sizes: Vec<u64> = std::fs::read_dir(s.volume.join(".kv/default"))
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .collect();
        assert_eq!(
            s.quota.usage(),
            (sizes.iter().sum(), 2 + sizes.len() as u64),
            "usage is what is on disk"
        );
    }

    /// Reads share a budget of the object's memory limit, so a batch can't
    /// make the host hold more than the guest could.
    #[test]
    fn reads_are_bounded_by_the_memory_limit() {
        let mut s = state(Limits {
            memory_mb: 1,
            ..Limits::default()
        });
        let b = ok(s.open_bucket(""));
        std::fs::write(ok(s.key_path(&b, "a")), vec![0; 600 << 10]).unwrap();
        std::fs::write(ok(s.key_path(&b, "whole")), vec![0; 1 << 20]).unwrap();
        std::fs::write(ok(s.key_path(&b, "over")), vec![0; (1 << 20) + 1]).unwrap();
        assert_eq!(get(&s, &b, "whole").map(|v| v.len()), Some(1 << 20));
        assert_eq!(
            why(s.get_value(&b, "over", &mut s.memory_budget())),
            "more than the object's memory limit"
        );
        let mut budget = s.memory_budget();
        ok(s.get_value(&b, "a", &mut budget));
        assert_eq!(budget, (1 << 20) - (600 << 10));
        assert!(s.get_value(&b, "a", &mut budget).is_err(), "the budget is shared");
    }

    /// Keys list sorted, a page at a time; anything in the directory that
    /// isn't a regular file named by a valid key's canonical encoding is
    /// skipped.
    #[test]
    fn list_pages_and_skips_foreign_files() {
        let mut s = state(Limits::default());
        let b = ok(s.open_bucket(""));
        let dir = s.volume.join(".kv/default");
        let mut want: Vec<String> = (0..2 * PAGE + 1).rev().map(|i| format!("k{i:05}")).collect();
        for k in &want {
            std::fs::write(dir.join(B64.encode(k)), b"").unwrap();
        }
        for junk in [".tmp", "QR", "QQ==", "_w", "a b", &B64.encode("x".repeat(MAX_KEY + 1))] {
            std::fs::write(dir.join(junk), b"").unwrap();
        }
        std::fs::create_dir(dir.join(B64.encode("dir"))).unwrap();
        std::os::unix::fs::symlink(dir.join(B64.encode("k00000")), dir.join(B64.encode("link"))).unwrap();
        want.sort();

        let mut got = vec![];
        let mut cursor = None;
        let mut cursors = vec![];
        loop {
            let page = ok(s.list(&b, cursor));
            assert!(page.keys.len() <= PAGE);
            got.extend(page.keys);
            cursors.push(page.cursor);
            cursor = page.cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(cursors, [Some(PAGE as u64), Some(2 * PAGE as u64), None]);
        assert_eq!(got, want);
        for past in [want.len() as u64, u64::MAX] {
            let page = ok(s.list(&b, Some(past)));
            assert!(page.keys.is_empty() && page.cursor.is_none());
        }
    }

    /// An absent key counts as 0; anything but a decimal u64 is not a
    /// number, and overflow is an error that leaves the value alone.
    #[test]
    fn increment_counts_in_decimal() {
        let mut s = state(Limits::default());
        let b = ok(s.open_bucket(""));
        assert_eq!(ok(s.increment_value(&b, "n", 5)), 5);
        assert_eq!(ok(s.increment_value(&b, "n", 2)), 7);
        assert_eq!(get(&s, &b, "n").unwrap(), b"7");
        assert_eq!(ok(s.increment_value(&b, "z", 0)), 0);
        for bad in ["abc", "-1", " 1", "1.0", "", "0x10", "\u{ff}"] {
            ok(s.set_value(&b, "bad", bad.as_bytes()));
            assert_eq!(why(s.increment_value(&b, "bad", 1)), "not a number", "{bad:?}");
        }
        let max = u64::MAX.to_string();
        ok(s.set_value(&b, "max", max.as_bytes()));
        assert_eq!(why(s.increment_value(&b, "max", 1)), "overflow");
        assert_eq!(get(&s, &b, "max").unwrap(), max.as_bytes());
        assert_eq!(ok(s.increment_value(&b, "max", 0)), u64::MAX);
    }

    /// A batch names at most a page of keys.
    #[test]
    fn batches_are_bounded() {
        assert!(check_batch(PAGE).is_ok());
        assert_eq!(why(check_batch(PAGE + 1)), "at most 1000 keys per batch");
    }

    /// A bucket handle that is no longer in the table traps rather than
    /// reaching the guest as an error.
    #[test]
    fn stale_buckets_trap() {
        let mut s = state(Limits::default());
        let b = ok(s.open_bucket(""));
        let rep = b.rep();
        s.table.delete(b).unwrap();
        let stale = Resource::<Bucket>::new_borrow(rep);
        assert!(done(s.key_path(&stale, "k")).is_err());
        let b = ok(s.open_bucket(""));
        assert!(matches!(done(s.key_path(&b, "")), Ok(Err(Error::Other(_)))));
    }
}
