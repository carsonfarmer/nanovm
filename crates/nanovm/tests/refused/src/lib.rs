//! Test-only components for deploy-time validation; never part of the
//! published guest workspace. This library is the control: an HTTP service
//! importing only what nanovm offers, which must load. Each example adds one
//! import nanovm refuses (or drops the handler export) and must not.
use util::wasip3;
use wasip3::http::types::{ErrorCode, Request, Response};

wasip3::http::service::export!(Allowed);
struct Allowed;

impl wasip3::exports::http::handler::Guest for Allowed {
    async fn handle(_: Request) -> Result<Response, ErrorCode> {
        util::text(200, "allowed")
    }
}
