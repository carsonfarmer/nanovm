//! The app's config (`wasi:config/store`): the `config` table of its
//! `nanovm.json`, and the secrets the node's operator gives it (those its
//! manifest names in `secrets`) over it; the same for every object.

mod bindings {
    wit_bindgen::generate!({ path: "../../wit/config", world: "wasi:config/imports", generate_all });
}

pub use bindings::wasi::config::store::{Error, get_all};

/// Config value `key`, if the app has one.
pub fn get(key: &str) -> Result<Option<String>, Error> {
    bindings::wasi::config::store::get(key)
}
