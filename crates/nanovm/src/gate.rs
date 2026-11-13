//! What one client, and one app, may ask of a public listener: the proxy's,
//! or a node's when clients reach it directly. Each client address may send
//! `rate` requests a second and each app receive `app_rate`, both with
//! bursts of twice that (GCRA, from `governor`); past either a request is
//! answered 429. Each client address may hold `connections_per_client`
//! connections, past which one is answered 503; past `connections` in all,
//! new ones wait in the listen backlog. Both answers name a `retry-after`.
//!
//! Behind a TLS terminator, or behind the proxy, requests come from a few
//! peers that forward everyone's: those in `trusted`. Their own requests and
//! connections aren't limited, and the client of a request one forwards is
//! the last address in its `x-forwarded-for` that isn't trusted, so what a
//! client writes there itself is left of its own address and never read. An
//! IPv6 client counts by its /64, which one user usually holds whole.

use std::collections::HashMap;
use std::hash::Hash;
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use governor::{DefaultKeyedRateLimiter, Quota};
use http::{Request, Response};
use hyper::body::Incoming;
use ipnet::IpNet;
use parking_lot::Mutex;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::config::ClientLimits;
use crate::object::{RespBody, error_response};

/// The address a request's connection came from, which `serve` gives it.
#[derive(Clone, Copy, Debug)]
pub struct Peer(pub IpAddr);

pub struct Gate {
    trusted: Vec<IpNet>,
    clients: Option<DefaultKeyedRateLimiter<IpAddr>>,
    apps: Option<DefaultKeyedRateLimiter<String>>,
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

/// A limiter of `rate` a second per key, with bursts of twice that; none for 0.
fn limiter<K: Hash + Eq + Clone>(rate: u32) -> Option<DefaultKeyedRateLimiter<K>> {
    let (rate, burst) = (NonZeroU32::new(rate)?, NonZeroU32::new(rate.saturating_mul(2))?);
    Some(DefaultKeyedRateLimiter::keyed(Quota::per_second(rate).allow_burst(burst)))
}

/// `ip` as one client: an IPv4-mapped address as IPv4, an IPv6 one as its /64.
fn key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & !0u128 << 64).into()),
        v4 => v4,
    }
}

/// A refusal that says when to try again, in whole seconds.
fn refusal(status: u16, kind: &str, why: &str, after: Duration) -> Response<RespBody> {
    let mut res = error_response(status, kind, why);
    let secs = after.as_millis().div_ceil(1000).max(1) as u64;
    res.headers_mut().insert(http::header::RETRY_AFTER, secs.into());
    res
}

impl Gate {
    /// Call within a Tokio runtime.
    pub fn new(cfg: &ClientLimits) -> anyhow::Result<Arc<Gate>> {
        let gate = Arc::new(Gate {
            trusted: trusted(&cfg.trusted)?,
            clients: limiter(cfg.rate),
            apps: limiter(cfg.app_rate),
            connections: Arc::new(Semaphore::new(if cfg.connections == 0 { Semaphore::MAX_PERMITS } else { cfg.connections })),
            per_client: if cfg.connections_per_client == 0 { usize::MAX } else { cfg.connections_per_client },
            open: Mutex::default(),
        });
        // Forgets the clients and apps whose limits have refilled.
        let weak = Arc::downgrade(&gate);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let Some(gate) = weak.upgrade() else { return };
                for l in &gate.clients { l.retain_recent(); l.shrink_to_fit() }
                for l in &gate.apps { l.retain_recent(); l.shrink_to_fit() }
            }
        });
        Ok(gate)
    }

    fn trusts(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.trusted.iter().any(|n| n.contains(&ip))
    }

    /// Who sent `req`, unless that is a trusted peer (then it isn't
    /// limited): its connection's peer, or, from a trusted peer, the last
    /// address in `x-forwarded-for` that isn't trusted. One that doesn't
    /// read as an address counts as the hop that sent it.
    pub fn client<B>(&self, req: &Request<B>) -> Option<IpAddr> {
        let mut client = req.extensions().get::<Peer>()?.0;
        let hops = req.headers().get_all("x-forwarded-for").iter().flat_map(|v| v.to_str().unwrap_or("?").split(','));
        let hops: Vec<_> = hops.collect();
        for hop in hops.iter().rev() {
            if !self.trusts(client) {
                break;
            }
            let hop = hop.trim();
            match hop.parse().or_else(|_| hop.parse::<SocketAddr>().map(|s| s.ip())) {
                Ok(ip) => client = ip,
                Err(_) => return Some(key(client)),
            }
        }
        (!self.trusts(client)).then(|| key(client))
    }

    /// Admits a request from `client` (see [`Gate::client`]; none is
    /// trusted) to `app` (none: not known yet), or refuses it with 429 when
    /// either's rate is spent.
    pub fn admit(&self, client: Option<IpAddr>, app: Option<&String>) -> Result<(), Response<RespBody>> {
        let spent = match (client, app) {
            (None, _) => return Ok(()),
            (Some(client), None) => self.clients.as_ref().map(|l| l.check_key(&client).map_err(|e| ("client", e))),
            (Some(_), Some(app)) => self.apps.as_ref().map(|l| l.check_key(app).map_err(|e| ("app", e))),
        };
        let Some(Err((who, until))) = spent else { return Ok(()) };
        let why = format!("this {who} has sent more requests a second than it may; try again later");
        Err(refusal(429, "rate-limited", &why, until.wait_time_from(Instant::now())))
    }

    /// One of `peer`'s connections, if it may open another (a trusted peer
    /// always may); dropping it frees the slot.
    fn hold(self: &Arc<Self>, peer: IpAddr) -> Option<Held> {
        let (mut open, client) = (self.open.lock(), key(peer));
        let n = open.entry(client).or_default();
        (*n < self.per_client || self.trusts(peer)).then(|| {
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

/// Serves HTTP/1.1 on `listener` within `gate`'s connection limits, giving
/// each request its [`Peer`]. See `server::serve_conn` for `header_timeout`.
pub async fn serve<F, Fut>(gate: Arc<Gate>, listener: TcpListener, header_timeout: Option<Duration>, f: F) -> anyhow::Result<()>
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<RespBody>> + Send + 'static,
{
    loop {
        let slot = gate.connections.clone().acquire_owned().await?;
        let sock = crate::server::accept(&listener).await;
        let Ok(peer) = sock.peer_addr().map(|a| a.ip()) else { continue };
        match gate.hold(peer) {
            Some(held) => {
                let f = f.clone();
                crate::server::serve_conn(sock, header_timeout, move |mut req| {
                    let _ = (&slot, &held);
                    req.extensions_mut().insert(Peer(peer));
                    f(req)
                })
            }
            None => crate::server::serve_conn(sock, Some(REFUSE_TIMEOUT), move |_| {
                let _ = &slot;
                let mut res = refusal(503, "too-many-connections", "this client has as many connections open as it may", Duration::ZERO);
                res.headers_mut().insert(http::header::CONNECTION, http::HeaderValue::from_static("close"));
                async { res }
            }),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(rate: u32, app_rate: u32, trusted: &[&str]) -> Arc<Gate> {
        let cfg = ClientLimits {
            rate,
            app_rate,
            trusted: trusted.iter().map(|s| s.to_string()).collect(),
            ..ClientLimits::default()
        };
        Gate::new(&cfg).unwrap()
    }

    fn from(peer: &str, forwarded: Option<&str>) -> Request<()> {
        let mut req = Request::new(());
        req.extensions_mut().insert(Peer(peer.parse().unwrap()));
        if let Some(f) = forwarded {
            req.headers_mut().insert("x-forwarded-for", f.parse().unwrap());
        }
        req
    }

    /// A client is its peer unless the peer is trusted; then it is the last
    /// untrusted hop its forwarders name, whatever the client wrote to the
    /// left of that. Trusted clients aren't limited; IPv6 clients count by
    /// their /64.
    #[tokio::test]
    async fn clients_are_named_only_by_trusted_forwarders() {
        let g = gate(1, 1, &["10.0.0.0/8", "::1"]);
        let client = |peer, fwd| g.client(&from(peer, fwd)).map(|ip| ip.to_string());
        // Not trusted: what it says it forwards is ignored.
        assert_eq!(client("203.0.113.9", Some("198.51.100.1")).as_deref(), Some("203.0.113.9"));
        // Trusted: the last hop it names that isn't trusted.
        assert_eq!(client("10.1.2.3", Some("198.51.100.1, 203.0.113.9")).as_deref(), Some("203.0.113.9"));
        assert_eq!(client("10.1.2.3", Some("198.51.100.1, 203.0.113.9, 10.0.0.7")).as_deref(), Some("203.0.113.9"));
        assert_eq!(client("10.1.2.3", Some("203.0.113.9:4711")).as_deref(), Some("203.0.113.9"));
        // A trusted peer on its own, or forwarding only trusted hops.
        assert_eq!(client("10.1.2.3", None), None);
        assert_eq!(client("::1", Some("10.0.0.7")), None);
        // An unreadable hop counts as the forwarder that sent it.
        assert_eq!(client("10.1.2.3", Some("203.0.113.9, junk")).as_deref(), Some("10.1.2.3"));
        assert_eq!(client("2001:db8:1:2:3:4:5:6", None).as_deref(), Some("2001:db8:1:2::"));
        assert_eq!(client("::ffff:203.0.113.9", None).as_deref(), Some("203.0.113.9"));
    }

    /// Past its burst a client, or an app, is refused with a retry-after;
    /// another client or app is not; a trusted client never is.
    #[tokio::test]
    async fn rates_are_per_client_and_per_app() {
        let g = gate(5, 20, &[]);
        let c = |s: &str| Some(s.parse::<IpAddr>().unwrap());
        for _ in 0..10 {
            g.admit(c("192.0.2.1"), None).unwrap();
        }
        let res = g.admit(c("192.0.2.1"), None).unwrap_err();
        assert_eq!(res.status(), 429);
        assert_eq!(res.headers()["retry-after"], "1");
        g.admit(c("192.0.2.2"), None).unwrap();
        g.admit(None, None).unwrap();
        let (a, b) = ("a".to_string(), "b".to_string());
        for _ in 0..40 {
            g.admit(c("192.0.2.3"), Some(&a)).unwrap();
        }
        assert_eq!(g.admit(c("192.0.2.4"), Some(&a)).unwrap_err().status(), 429);
        g.admit(c("192.0.2.4"), Some(&b)).unwrap();
        g.admit(None, Some(&a)).unwrap();
        // 0 is no limit.
        let open = gate(0, 0, &[]);
        for _ in 0..1000 {
            open.admit(c("192.0.2.1"), Some(&a)).unwrap();
        }
    }

    /// A client gets `connections_per_client` slots; a trusted peer any
    /// number; a closed one is free again.
    #[tokio::test]
    async fn connections_are_capped_per_client() {
        let cfg = ClientLimits { connections_per_client: 2, trusted: vec!["10.0.0.1".into()], ..ClientLimits::default() };
        let g = Gate::new(&cfg).unwrap();
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let (a, b) = (g.hold(ip("192.0.2.1")).unwrap(), g.hold(ip("192.0.2.1")).unwrap());
        assert!(g.hold(ip("192.0.2.1")).is_none());
        let held = (g.hold(ip("2001:db8::1")).unwrap(), g.hold(ip("2001:db8::2")).unwrap());
        assert!(g.hold(ip("2001:db8::3")).is_none(), "one /64");
        drop(held);
        let trusted: Vec<_> = (0..10).map(|_| g.hold(ip("10.0.0.1")).unwrap()).collect();
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
