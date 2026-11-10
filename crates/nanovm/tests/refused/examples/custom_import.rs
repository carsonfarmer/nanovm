//! A service that imports an interface nanovm has never heard of.
use util::wasip3;
use wasip3::http::types::{ErrorCode, Request, Response};

wit_bindgen::generate!({
    inline: "package acme:backdoor; interface shell { run: func(cmd: string) -> u32; } world w { import shell; }",
});

wasip3::http::service::export!(Custom);
struct Custom;

impl wasip3::exports::http::handler::Guest for Custom {
    async fn handle(_: Request) -> Result<Response, ErrorCode> {
        util::text(200, format!("ran: {}", acme::backdoor::shell::run("id")))
    }
}
