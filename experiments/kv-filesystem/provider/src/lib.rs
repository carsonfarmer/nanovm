//! ABI feasibility probe, not a complete KV provider. No host-storage backchannel.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use util::wasip3::wit_bindgen::block_on;

mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit/keyvalue",
        inline: "package nanovm-experiment:kv; world provider { export wasi:keyvalue/store@0.2.0-draft; }",
        world: "provider",
    });
}
use bindings::exports::wasi::keyvalue::store::{Bucket, Error, Guest, GuestBucket, KeyResponse};
bindings::export!(Provider with_types_in bindings);

struct Provider;
pub struct Store;

impl Guest for Provider {
    type Bucket = Store;

    fn open(identifier: String) -> Result<Bucket, Error> {
        if !identifier.is_empty() {
            // Distinguishes this composed provider from the native host in the control.
            return Err(Error::Other("W18 composed provider".into()));
        }
        Ok(Bucket::new(Store))
    }
}

fn path(key: &str) -> Result<String, Error> {
    if key.len() > 180 {
        return Err(Error::Other("key too long".into()));
    }
    Ok(format!(".kv/default/{}", URL_SAFE_NO_PAD.encode(key)))
}

impl GuestBucket for Store {
    fn get(&self, key: String) -> Result<Option<Vec<u8>>, Error> {
        let path = path(&key)?;
        block_on(async {
            util::read_file("/data", &path)
                .await
                .map(Some)
                .map_err(|e| Error::Other(format!("{e:?}")))
        })
    }

    fn set(&self, key: String, value: Vec<u8>) -> Result<(), Error> {
        let path = path(&key)?;
        block_on(async {
            util::write_file("/data", &path, value)
                .await
                .map_err(|e| Error::Other(format!("{e:?}")))
        })
    }

    fn delete(&self, _key: String) -> Result<(), Error> {
        Err(Error::Other("ABI probe: delete not implemented".into()))
    }

    fn exists(&self, _key: String) -> Result<bool, Error> {
        Err(Error::Other("ABI probe: exists not implemented".into()))
    }

    fn list_keys(&self, _cursor: Option<u64>) -> Result<KeyResponse, Error> {
        Err(Error::Other("ABI probe: listing not implemented".into()))
    }
}
