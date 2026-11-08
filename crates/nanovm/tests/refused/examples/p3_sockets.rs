//! A service that creates a socket through `wasi:sockets/types@0.3`.
use util::wasip3;
use wasip3::http::types::{ErrorCode, Request, Response};
use wasip3::sockets::types::{IpAddressFamily, TcpSocket};

wasip3::http::service::export!(Sockets);
struct Sockets;

impl wasip3::exports::http::handler::Guest for Sockets {
    async fn handle(_: Request) -> Result<Response, ErrorCode> {
        let created = TcpSocket::create(IpAddressFamily::Ipv4).is_ok();
        util::text(200, format!("created: {created}"))
    }
}
