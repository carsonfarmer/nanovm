//! What one client may ask of a public listener: the proxy's, or a node's
//! when clients reach it directly. Each client address may hold
//! `connections_per_client` connections, past which one is answered 503
//! with a `retry-after`; past `connections` in all, new ones wait in the
//! listen backlog. An IPv6 client counts by its /64, which one user usually
//! holds whole. Peers in `trusted` forward other clients' connections (a TLS
//! terminator in front of the proxy, the proxy in front of a node), so
//! theirs aren't capped per client. Request rates are left to the edge in
//! front (Caddy, a CDN).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use http::{HeaderValue, Request, Response};
use hyper::body::Incoming;
use ipnet::IpNet;
use parking_lot::Mutex;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::config::ClientLimits;
use crate::object::{RespBody, error_response};

pub struct Gate {
    trusted: Vec<IpNet>,
    connections: Arc<Semaphore>,
    per_client: usize,
    open: Mutex<HashMap<IpAddr, usize>>,
}

/// How long a client over its connections has to send the request it is
/// refused.
const REFUSE_TIMEOUT: Duration = Duration::from_secs(1);

/// `trusted` as address ranges; a bare address is a range of one.
pub fn trusted(trusted: &[String]) -> anyhow::Result<Vec<IpNet>> {
    let parse = |s: &String| s.parse().or_else(|_| s.parse::<IpAddr>().map(IpNet::from));
    trusted.iter().map(|s| parse(s).map_err(|_| anyhow::anyhow!("clients.trusted: {s:?} is not an address or CIDR range"))).collect()
}

/// `ip` as one client: an IPv4-mapped address as IPv4, an IPv6 one as its /64.
fn key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & !0u128 << 64).into()),
        v4 => v4,
    }
}

impl Gate {
    pub fn new(cfg: &ClientLimits) -> anyhow::Result<Arc<Gate>> {
        Ok(Arc::new(Gate {
            trusted: trusted(&cfg.trusted)?,
            connections: Arc::new(Semaphore::new(if cfg.connections == 0 { Semaphore::MAX_PERMITS } else { cfg.connections })),
            per_client: if cfg.connections_per_client == 0 { usize::MAX } else { cfg.connections_per_client },
            open: Mutex::default(),
        }))
    }

    /// One of `peer`'s connections, if it may open another (a trusted peer
    /// always may); dropping it frees the slot.
    fn hold(self: &Arc<Self>, peer: IpAddr) -> Option<Held> {
        let (mut open, client) = (self.open.lock(), key(peer));
        let n = open.entry(client).or_default();
        let trusted = self.trusted.iter().any(|t| t.contains(&peer.to_canonical()));
        (*n < self.per_client || trusted).then(|| {
            *n += 1;
            Held(self.clone(), client)
        })
    }
}

struct Held(Arc<Gate>, IpAddr);

impl Drop for Held {
    fn drop(&mut self) {
        let mut open = self.0.open.lock();
        if let std::collections::hash_map::Entry::Occupied(mut n) = open.entry(self.1) {
            *n.get_mut() -= 1;
            if *n.get() == 0 {
                n.remove();
            }
        }
    }
}

/// Serves HTTP/1.1 on `listener` within `gate`'s connection limits. See
/// `server::serve_conn` for `header_timeout`.
pub async fn serve<F, Fut>(gate: Arc<Gate>, listener: TcpListener, header_timeout: Option<Duration>, f: F) -> anyhow::Result<()>
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<RespBody>> + Send + 'static,
{
    loop {
        let slot = gate.connections.clone().acquire_owned().await?;
        let (sock, peer) = crate::server::accept(&listener).await;
        match gate.hold(peer.ip()) {
            Some(held) => crate::server::serve_conn(sock, header_timeout, (slot, held), f.clone()),
            None => crate::server::serve_conn(sock, Some(REFUSE_TIMEOUT), slot, |_| {
                let mut res = error_response(503, "too-many-connections", "this client has as many connections open as it may");
                res.headers_mut().insert(http::header::RETRY_AFTER, HeaderValue::from_static("1"));
                res.headers_mut().insert(http::header::CONNECTION, HeaderValue::from_static("close"));
                async { res }
            }),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client gets `connections_per_client` slots, an IPv6 one per /64
    /// and an IPv4-mapped one as its IPv4 address; a trusted peer any
    /// number; a closed one is free again.
    #[tokio::test]
    async fn connections_are_capped_per_client() {
        let cfg = ClientLimits { connections_per_client: 2, trusted: vec!["10.0.0.0/8".into()], ..ClientLimits::default() };
        let g = Gate::new(&cfg).unwrap();
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let (a, b) = (g.hold(ip("192.0.2.1")).unwrap(), g.hold(ip("192.0.2.1")).unwrap());
        assert!(g.hold(ip("192.0.2.1")).is_none());
        assert!(g.hold(ip("::ffff:192.0.2.1")).is_none(), "IPv4-mapped");
        let held = (g.hold(ip("2001:db8::1")).unwrap(), g.hold(ip("2001:db8::2")).unwrap());
        assert!(g.hold(ip("2001:db8::3")).is_none(), "one /64");
        drop(held);
        let trusted: Vec<_> = (0..10).map(|i| g.hold(ip(["10.0.0.1", "::ffff:10.0.0.1"][i % 2])).unwrap()).collect();
        drop((a, trusted));
        let _c = g.hold(ip("192.0.2.1")).unwrap();
        drop(b);
        assert_eq!(g.open.lock().get(&ip("192.0.2.1")), Some(&1));
    }

    #[test]
    fn trusted_takes_addresses_and_ranges() {
        assert_eq!(trusted(&["10.0.0.0/8".into(), "::1".into()]).unwrap().len(), 2);
        assert!(trusted(&["10.0.0.0/33".into()]).is_err());
        assert!(trusted(&["proxy.internal".into()]).is_err());
    }
}
