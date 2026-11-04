//! The object's key-value store (`wasi:keyvalue`). It lives in the object's
//! volume and commits with the request, like a file; values count against the
//! disk budget, keys against `max_files`. The functions here use the default
//! bucket; [`store::open`] names others.

wit_bindgen::generate!({ path: "../../wit/keyvalue", world: "wasi:keyvalue/imports", generate_all });

pub use wasi::keyvalue::{atomics, batch, store};
pub use store::{Bucket, Error};

/// The default bucket.
pub fn bucket() -> Result<Bucket, Error> {
    store::open("")
}

pub fn get(key: &str) -> Result<Option<Vec<u8>>, Error> {
    bucket()?.get(key)
}

pub fn set(key: &str, value: impl AsRef<[u8]>) -> Result<(), Error> {
    bucket()?.set(key, value.as_ref())
}

pub fn delete(key: &str) -> Result<(), Error> {
    bucket()?.delete(key)
}

/// Adds `delta` to the number stored at `key` (0 if there is none): the new value.
pub fn increment(key: &str, delta: u64) -> Result<u64, Error> {
    atomics::increment(&bucket()?, key, delta)
}

/// Every key, in no particular order.
pub fn keys() -> Result<Vec<String>, Error> {
    let (bucket, mut keys, mut cursor) = (bucket()?, vec![], None);
    loop {
        let page = bucket.list_keys(cursor)?;
        keys.extend(page.keys);
        match page.cursor {
            Some(c) => cursor = Some(c),
            None => return Ok(keys),
        }
    }
}
