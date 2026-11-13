//! A service that creates a TCP socket through `wasi:sockets/*@0.2`, which
//! the WASIp2 linker nanovm uses defines: only the import check keeps it out.
use util::wasip3;
use wasip3::http::types::{ErrorCode, Request, Response};

wasip3::http::service::export!(Sockets);
struct Sockets;

impl wasip3::exports::http::handler::Guest for Sockets {
    async fn handle(_: Request) -> Result<Response, ErrorCode> {
        use wasip2::sockets::{network::IpAddressFamily, tcp_create_socket::create_tcp_socket};
        let created = create_tcp_socket(IpAddressFamily::Ipv4).is_ok();
        util::text(200, format!("created: {created}"))
    }
}
