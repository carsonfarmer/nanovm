//! A service that reads a file with `std::fs`, which on wasm32-wasip2
//! imports the WASIp2 filesystem (`wasi:filesystem/*@0.2`), not the quota'd
//! WASIp3 one.
use util::wasip3;
use wasip3::http::types::{ErrorCode, Request, Response};

wasip3::http::service::export!(Files);
struct Files;

impl wasip3::exports::http::handler::Guest for Files {
    async fn handle(_: Request) -> Result<Response, ErrorCode> {
        let read = std::fs::read("/etc/passwd").map(|b| b.len()).unwrap_or(0);
        util::text(200, format!("read: {read}"))
    }
}
