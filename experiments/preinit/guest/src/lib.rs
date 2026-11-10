//! A small Rust app for the pre-initialization spike: `GET /` answers its
//! object's count (from `/data/count`) and a lookup in a table the app
//! builds on first use; `POST /incr` bumps the count. With `--features
//! heavy` the table has 50,000 entries (a router, templates or a parsed
//! config); without, it is empty. `wizer-initialize` builds the table and
//! touches nothing on the host, so a pre-initialized build starts with it.
//! `GET /hash` hashes a fixed string with a fresh std `RandomState`: two
//! objects answer differently unless their hash keys are shared. With
//! `--features touch-data` the initializer also reads `/data`, which the
//! pre-initializer must refuse.

use std::collections::HashMap;
use std::sync::LazyLock;

use util::wasip3;
use util::{Req, json, read_req, text};
use wasip3::http::types::{ErrorCode, Request, Response};

wasip3::http::service::export!(App);

struct App;

const ENTRIES: u64 = if cfg!(feature = "heavy") { 50_000 } else { 0 };

static TABLE: LazyLock<HashMap<String, u64>> = LazyLock::new(|| {
    (0..ENTRIES).map(|i| (format!("route-{:x}", i.wrapping_mul(0x9e37_79b9_7f4a_7c15)), i)).collect()
});

impl wasip3::exports::http::handler::Guest for App {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let req: Req = read_req(request).await;
        let count = match util::read_file("/data", "count").await {
            Ok(b) => String::from_utf8_lossy(&b).trim().parse().unwrap_or(0u64),
            Err(_) => 0,
        };
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/") => {
                let key = format!("route-{:x}", 7u64.wrapping_mul(0x9e37_79b9_7f4a_7c15));
                json(200, format!("{{\"count\":{count},\"table\":{},\"seven\":{:?}}}", TABLE.len(), TABLE.get(&key)))
            }
            ("GET", "/hash") => {
                use std::hash::BuildHasher;
                json(200, format!("{{\"hash\":{}}}", std::collections::hash_map::RandomState::new().hash_one("x")))
            }
            ("POST", "/incr") => match util::write_file_atomic("/data", "count", (count + 1).to_string().into_bytes()).await {
                Ok(()) => json(200, format!("{{\"count\":{}}}", count + 1)),
                Err(e) => json(507, format!("{{\"error\":\"{}\"}}", util::fs_err(&e))),
            },
            _ => text(404, "not found"),
        }
    }
}

/// Run once at build time by experiments/preinit/tool; every host import
/// traps meanwhile.
mod init {
    wit_bindgen::generate!({
        inline: "package nanovm:preinit; world init { export wizer-initialize: func(); }",
    });
    export!(Init);
    struct Init;
    impl Guest for Init {
        fn wizer_initialize() {
            std::sync::LazyLock::force(&super::TABLE);
            #[cfg(feature = "touch-data")]
            let _ = util::wasip3::wit_bindgen::block_on(util::read_file("/data", "count"));
        }
    }
}
