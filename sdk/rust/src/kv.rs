//! The object's key-value store: Spin's `spin:key-value` interface, served
//! by a Wasm provider composed into the app (`tools/build-app.sh` does it)
//! over the object's own volume. A store is files under `/data/.kv/`, so it
//! commits with the request like every other change, values count against
//! `disk_mb` and keys against `max_files`.
//!
//! The functions here use the store `default`; [`open`] names others.
//! Keys are 1–180 bytes, values at most 1 MiB.

mod bindings {
    wit_bindgen::generate!({ path: "../../components/keyvalue/wit", world: "client", generate_all });
}

pub use bindings::spin::key_value::key_value::{Error, Store};

thread_local! {
    // Opened at first use (at run time, never in a pre-initialized
    // snapshot) and kept for the instance's life: a store is a label, not
    // an open directory.
    static DEFAULT: std::cell::OnceCell<&'static Store> = const { std::cell::OnceCell::new() };
}

/// The store `default`.
pub async fn store() -> Result<&'static Store, Error> {
    if let Some(s) = DEFAULT.with(|d| d.get().copied()) {
        return Ok(s);
    }
    let s: &'static Store = Box::leak(Box::new(open("default").await?));
    Ok(DEFAULT.with(|d| *d.get_or_init(|| s)))
}

/// The store `label`: 1–64 ASCII letters, digits, `_` or `-`.
pub async fn open(label: &str) -> Result<Store, Error> {
    Store::open(label.into()).await
}

/// The value at `key`, if there is one.
pub async fn get(key: &str) -> Result<Option<Vec<u8>>, Error> {
    store().await?.get(key.into()).await
}

/// [`get`], as text (invalid UTF-8 replaced).
pub async fn get_text(key: &str) -> Result<Option<String>, Error> {
    Ok(get(key).await?.map(|v| String::from_utf8_lossy(&v).into_owned()))
}

pub async fn set(key: &str, value: impl Into<Vec<u8>>) -> Result<(), Error> {
    store().await?.set(key.into(), value.into()).await
}

/// Removes `key`; removing a missing key is not an error.
pub async fn delete(key: &str) -> Result<(), Error> {
    store().await?.delete(key.into()).await
}

pub async fn exists(key: &str) -> Result<bool, Error> {
    store().await?.exists(key.into()).await
}

/// Adds `delta` to the decimal number at `key` (0 if there is none): the new
/// value. An object serves one request at a time and the request commits as
/// a whole, so no other change can come between the read and the write.
pub async fn increment(key: &str, delta: i64) -> Result<i64, Error> {
    let n = get_text(key).await?.and_then(|n| n.trim().parse::<i64>().ok()).unwrap_or(0) + delta;
    set(key, n.to_string()).await?;
    Ok(n)
}

/// Every key in the store, in no particular order.
pub async fn keys() -> Result<Vec<String>, Error> {
    let (keys, done) = store().await?.get_keys().await;
    let keys = keys.collect().await;
    done.await?;
    Ok(keys)
}
