//! Outbound HTTP for guests: a per-app host allowlist, a per-object
//! connection semaphore, and an IP filter applied to the resolved address
//! (so DNS cannot point an allowed name at loopback, link-local metadata
//! endpoints or private ranges). The connection goes to the address that was
//! checked, never re-resolved.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use http::uri::Scheme;
use http_body_util::BodyExt;
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use wasmtime_wasi_http::{Error, RequestOptions, WasiBody, WasiHttpHooks};

use crate::metrics::Metrics;

#[derive(Debug, Clone)]
enum HostPat {
    Exact(String),
    /// `*.example.com` matches `a.example.com`, not `example.com`.
    Suffix(String),
    /// An IP literal: public, unless in dev mode.
    Ip(IpAddr),
}

#[derive(Debug, Clone)]
struct Rule {
    host: HostPat,
    port: Option<u16>,
}

#[derive(Debug, Default)]
pub struct EgressPolicy {
    rules: Vec<Rule>,
}

impl EgressPolicy {
    /// An app's list. Its author is the tenant, so it may name no private
    /// address, unless `dev`.
    pub fn parse(entries: &[String], dev: bool) -> anyhow::Result<Self> {
        let mut rules = Vec::new();
        for e in entries {
            let e = e.trim().to_ascii_lowercase();
            let (wild, rest) = e.strip_prefix("*.").map_or((false, e.as_str()), |r| (true, r));
            // `host`, `host:port` or `[v6]:port`.
            let a: http::uri::Authority = rest.parse().map_err(|_| anyhow::anyhow!("bad egress entry {e:?}"))?;
            // A port that isn't a u16 would otherwise read as "any port".
            let named = a.as_str().rsplit_once(']').map_or(a.as_str(), |(_, p)| p).contains(':');
            anyhow::ensure!(!named || a.port_u16().is_some(), "bad port in egress entry {e:?}");
            let host = a.host().trim_start_matches('[').trim_end_matches(']');
            let host = match host.parse::<IpAddr>() {
                _ if wild => HostPat::Suffix(format!(".{host}")),
                Ok(ip) if dev || is_global(ip) => HostPat::Ip(ip),
                Ok(_) => anyhow::bail!("egress entry {e:?} is a private address"),
                Err(_) => HostPat::Exact(host.to_string()),
            };
            rules.push(Rule {
                host,
                port: a.port_u16(),
            });
        }
        Ok(EgressPolicy { rules })
    }

    fn matches(&self, host: &str, port: u16) -> Option<&Rule> {
        let host = host.to_ascii_lowercase();
        let ip = host.parse::<IpAddr>().ok();
        self.rules.iter().find(|r| {
            r.port.is_none_or(|p| p == port)
                && match &r.host {
                    HostPat::Exact(h) => *h == host,
                    HostPat::Suffix(s) => host.ends_with(s.as_str()),
                    HostPat::Ip(a) => ip == Some(*a),
                }
        })
    }
}

/// True for addresses reachable on the public internet.
pub fn is_global(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_global_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_global_v4(v4);
            }
            let s = v6.segments();
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link local
                || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
                || (s[0] == 0x0064 && s[1] == 0xff9b) // NAT64
                || s[0] == 0x2002) // 6to4
        }
    }
}

fn is_global_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // CGNAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // IETF protocol assignments
        || (o[0] == 198 && (o[1] & 0xfe) == 18) // benchmarking
        || o[0] >= 240)
}

/// A worker's link to its supervisor, which makes the worker's outbound
/// requests on its behalf.
pub type Upstream = Arc<crate::link::Dialer<WasiBody>>;

/// Where an app's outbound requests go.
#[derive(Clone)]
pub enum Route {
    /// Checked and sent from this process (in-process nodes: tests, bench).
    Direct(Arc<EgressPolicy>),
    /// Forwarded to the supervisor, which holds the policy (jailed workers).
    Supervisor(Upstream),
}

/// Marks a supervisor's answer to a forwarded request: `ok`, or the name of
/// the `Error` the request failed with. Inserting it replaces any copy the
/// upstream sent, so the worker can trust it.
pub(crate) const VERDICT: &str = "x-nanovm-egress";
const CONNECT_TIMEOUT: &str = "x-nanovm-connect-timeout-ms";
const FIRST_BYTE_TIMEOUT: &str = "x-nanovm-first-byte-timeout-ms";

/// Where an object calls another of its app: `http://nanovm/objects/{id}/…`.
const CALLS: &str = "/objects/";

pub struct EgressHooks {
    pub route: Route,
    /// The node, for calls to other objects of the app.
    pub node: std::sync::Weak<crate::table::Node>,
    pub sem: Arc<Semaphore>,
    pub metrics: Arc<Metrics>,
    pub obj: Arc<crate::object::ObjShared>,
    /// Publishes to the object's sockets, held until its request commits.
    pub held: Arc<parking_lot::Mutex<Vec<http::Request<bytes::Bytes>>>>,
    pub publish_max: usize,
}

type IoFuture = Box<dyn Future<Output = Result<(), Error>> + Send>;

impl WasiHttpHooks for EgressHooks {
    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
        _fut: Box<dyn Future<Output = Result<(), Error>> + Send>,
    ) -> Box<dyn Future<Output = Result<(http::Response<WasiBody>, IoFuture), Error>> + Send> {
        let route = self.route.clone();
        let sem = self.sem.clone();
        let metrics = self.metrics.clone();
        let obj = self.obj.clone();
        if request.uri().host() == Some(crate::ws::PUBLISH_HOST) && request.uri().path().starts_with(CALLS) {
            let node = self.node.clone();
            return Box::new(async move { Ok((call(node, sem, obj, request).await?, Box::new(async { Ok(()) }) as IoFuture)) });
        }
        if request.uri().host() == Some(crate::ws::PUBLISH_HOST) && matches!(route, Route::Supervisor(_)) {
            let (held, max) = (self.held.clone(), self.publish_max);
            return Box::new(async move { Ok((hold(request, &obj, &held, max).await, Box::new(async { Ok(()) }) as IoFuture)) });
        }
        Box::new(async move {
            let target = request.uri().clone();
            let res = async {
                let permit = sem.try_acquire_owned().map_err(|_| Error::ConnectionLimitReached)?;
                let (res, conn) = match route {
                    Route::Direct(policy) => send(request, options, &policy).await?,
                    Route::Supervisor(up) => forward(request, options, up).await?,
                };
                // The permit lives as long as the response body, so a guest
                // holding bodies open holds that many connections.
                let res = res.map(|b| {
                    b.map_frame(move |f| {
                        let _ = &permit;
                        f
                    })
                    .boxed_unsync()
                });
                let io: IoFuture = Box::new(conn);
                Ok((res, io))
            }
            .await;
            if let Err(e) = &res {
                metrics.egress_denied.fetch_add(1, Relaxed);
                metrics.event("egress-denied", &obj.app, &obj.id, format!("{target} → {e}"));
            } else {
                metrics.egress_allowed.fetch_add(1, Relaxed);
            }
            res
        })
    }
}

/// Sends `req`, for `http://nanovm/objects/{id}/{path}`, to object `id` of
/// the caller's app as a request for `/{path}`, through its queue like any
/// other request. The callee commits before it answers, so the caller sees
/// only durable changes. A call that would wait on its own caller, directly
/// or through other calls, is refused with 508: one request at a time per
/// object would deadlock it.
async fn call(
    node: std::sync::Weak<crate::table::Node>,
    sem: Arc<Semaphore>,
    obj: Arc<crate::object::ObjShared>,
    req: http::Request<WasiBody>,
) -> Result<http::Response<WasiBody>, Error> {
    let node = node.upgrade().ok_or(Error::ConnectionTerminated)?;
    let (mut parts, body) = req.into_parts();
    let target = parts.uri.clone();
    let rest = target.path_and_query().map_or("", |p| p.as_str()).strip_prefix(CALLS).unwrap_or("");
    let (id, path) = rest.split_at(rest.find(['/', '?']).unwrap_or(rest.len()));
    let to_guest = |res: http::Response<crate::object::RespBody>| {
        res.map(|b| b.map_err(|_| Error::ConnectionTerminated).boxed_unsync())
    };
    if !node.wait_for(&obj, id) {
        let why = format!("{id} is already waiting on {}", obj.id);
        return Ok(to_guest(crate::object::error_response(508, "call-cycle", &why)));
    }
    let _waiting = Waiting(obj.clone(), id.into());
    let permit = sem.try_acquire_owned().map_err(|_| Error::ConnectionLimitReached)?;
    let limit = (node.app.cfg.limits.max_request_body_kb << 10) as usize;
    let body = http_body_util::Limited::new(body, limit).collect().await;
    let body = body.map_err(|_| Error::HttpRequestBodySize(None))?.to_bytes();
    parts.uri = format!("http://{}/{}", crate::ws::PUBLISH_HOST, path.trim_start_matches('/'))
        .parse()
        .map_err(|_| Error::HttpRequestUriInvalid)?;
    let res = node.dispatch(id, http::Request::from_parts(parts, body)).await;
    // The permit lives as long as the response body, as for any request.
    Ok(to_guest(res.map(|b| {
        b.map_frame(move |f| {
            let _ = &permit;
            f
        })
        .boxed_unsync()
    })))
}

/// A call `.0` waits on, until it is answered or given up.
struct Waiting(Arc<crate::object::ObjShared>, Arc<str>);

impl Drop for Waiting {
    fn drop(&mut self) {
        let mut waiting = self.0.waiting_on.lock();
        if let Some(i) = waiting.iter().position(|id| *id == self.1) {
            waiting.swap_remove(i);
        }
    }
}

/// Holds a publish until the request commits, named with its object so it
/// reaches only that object's sockets; answers what the supervisor would.
async fn hold(
    req: http::Request<WasiBody>,
    obj: &crate::object::ObjShared,
    held: &parking_lot::Mutex<Vec<http::Request<bytes::Bytes>>>,
    max: usize,
) -> http::Response<WasiBody> {
    let (mut parts, body) = req.into_parts();
    let status = match http_body_util::Limited::new(body, max).collect().await.map(|b| b.to_bytes()) {
        Err(_) => 413,
        Ok(body) if !crate::ws::well_formed(&body) => 400,
        // What a request holds is bounded like one publish.
        Ok(body) if held.lock().iter().map(|r| r.body().len()).sum::<usize>() + body.len() > max => 413,
        Ok(body) => {
            parts.headers.insert(crate::ws::OBJECT_HEADER, obj.id.parse().expect("object ids are header-safe"));
            held.lock().push(http::Request::from_parts(parts, body));
            200
        }
    };
    let mut res = http::Response::new(http_body_util::Empty::new().map_err(|e| match e {}).boxed_unsync());
    *res.status_mut() = http::StatusCode::from_u16(status).expect("a valid status");
    res
}

type Sent = (http::Response<WasiBody>, futures::future::BoxFuture<'static, Result<(), Error>>);

/// Hands a request to the supervisor and maps its verdict back to the error
/// the guest would have seen from a direct send.
async fn forward(mut req: http::Request<WasiBody>, options: Option<RequestOptions>, up: Upstream) -> Result<Sent, Error> {
    let h = req.headers_mut();
    for (name, d) in [
        (CONNECT_TIMEOUT, options.and_then(|o| o.connect_timeout)),
        (FIRST_BYTE_TIMEOUT, options.and_then(|o| o.first_byte_timeout)),
    ] {
        h.remove(name);
        if let Some(d) = d {
            h.insert(name, d.as_millis().to_string().parse().unwrap());
        }
    }
    let mut res = up.send(req).await.map_err(|_| Error::ConnectionTerminated)?;
    let verdict = res.headers_mut().remove(VERDICT);
    match verdict.as_ref().map(|v| v.as_bytes()) {
        Some(b"ok") => Ok((
            res.map(|b| b.map_err(|_| Error::ConnectionTerminated).boxed_unsync()),
            Box::pin(async { Ok(()) }),
        )),
        Some(name) => Err(error_named(name)),
        None => Err(Error::ConnectionTerminated),
    }
}

/// The errors `send` returns that carry no payload, so they cross the
/// process boundary by name. Anything else arrives as `ConnectionTerminated`.
fn error_named(name: &[u8]) -> Error {
    use Error::*;
    [
        HttpRequestDenied,
        HttpRequestUriInvalid,
        DestinationIpProhibited,
        DestinationNotFound,
        DnsTimeout,
        ConnectionTimeout,
        ConnectionReadTimeout,
        ConnectionRefused,
    ]
    .into_iter()
    .find(|e| format!("{e:?}").as_bytes() == name)
    .unwrap_or(ConnectionTerminated)
}

/// The supervisor's side of [`forward`]: sends one request from a worker
/// under that worker's app policy, and answers with the upstream response
/// or the error, marked with [`VERDICT`].
pub async fn serve_forwarded(
    req: http::Request<hyper::body::Incoming>,
    policy: &EgressPolicy,
) -> http::Response<WasiBody> {
    let mut req = req.map(|b| b.map_err(Error::from).boxed_unsync());
    // Taken off the request, so they never reach the upstream.
    let mut ms = |name| {
        let v = req.headers_mut().remove(name)?.to_str().ok()?.parse().ok()?;
        Some(Duration::from_millis(v))
    };
    let options = RequestOptions {
        connect_timeout: ms(CONNECT_TIMEOUT),
        first_byte_timeout: ms(FIRST_BYTE_TIMEOUT),
        between_bytes_timeout: None,
    };
    match send(req, Some(options), policy).await {
        Ok((mut res, _conn)) => {
            res.headers_mut().insert(VERDICT, "ok".parse().unwrap());
            res
        }
        Err(e) => {
            let name = format!("{e:?}");
            let name = name.split(['(', ' ', '{']).next().unwrap_or("ConnectionTerminated");
            let mut res = http::Response::new(http_body_util::Empty::new().map_err(|e| match e {}).boxed_unsync());
            *res.status_mut() = http::StatusCode::BAD_GATEWAY;
            res.headers_mut().insert(VERDICT, name.parse().unwrap());
            res
        }
    }
}

/// Checks `req` against `policy` and the IP filter, then sends it. The
/// returned future finishes when the connection does.
async fn send(mut req: http::Request<WasiBody>, options: Option<RequestOptions>, policy: &EgressPolicy) -> Result<Sent, Error> {
    let uri = req.uri().clone();
    let tls = uri.scheme() == Some(&Scheme::HTTPS);
    let host = uri.host().ok_or(Error::HttpRequestUriInvalid)?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });

    let rule = policy.matches(host, port).ok_or(Error::HttpRequestDenied)?;

    let addr = match (&rule.host, host.parse::<IpAddr>()) {
        (HostPat::Ip(ip), _) => SocketAddr::new(*ip, port),
        (_, Ok(ip)) if is_global(ip) => SocketAddr::new(ip, port),
        (_, Ok(_)) => return Err(Error::DestinationIpProhibited),
        (_, Err(_)) => {
            let addrs: Vec<SocketAddr> =
                match tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host((host, port))).await {
                    Ok(Ok(a)) => a.collect(),
                    Ok(Err(_)) => return Err(Error::DestinationNotFound),
                    Err(_) => return Err(Error::DnsTimeout),
                };
            // Every resolved address must be public: a name that mixes in a
            // private address is treated as a rebinding attempt.
            if addrs.is_empty() {
                return Err(Error::DestinationNotFound);
            }
            if !addrs.iter().all(|a| is_global(a.ip())) {
                return Err(Error::DestinationIpProhibited);
            }
            addrs[0]
        }
    };

    let cap = |d: Option<Duration>, max: u64| d.unwrap_or(Duration::from_secs(max)).min(Duration::from_secs(max));
    let connect_timeout = cap(options.and_then(|o| o.connect_timeout), 10);
    let first_byte_timeout = cap(options.and_then(|o| o.first_byte_timeout), 30);

    let tcp = match tokio::time::timeout(connect_timeout, TcpStream::connect(addr)).await {
        Ok(s) => s.map_err(Error::Connect)?,
        Err(_) => return Err(Error::ConnectionTimeout),
    };
    let io: Box<dyn Stream> = if tls {
        let name = rustls::pki_types::ServerName::try_from(host.to_string())?;
        let tls = tokio::time::timeout(connect_timeout, TLS.connect(name, tcp));
        Box::new(tls.await.map_err(|_| Error::ConnectionTimeout)?.map_err(Error::Tls)?)
    } else {
        Box::new(tcp)
    };

    let (mut sender, conn) = tokio::time::timeout(
        connect_timeout,
        hyper::client::conn::http1::Builder::new().handshake(wasmtime_wasi_http::io::TokioIo::new(io)),
    )
    .await
    .map_err(|_| Error::ConnectionTimeout)??;

    *req.uri_mut() = http::Uri::builder()
        .path_and_query(uri.path_and_query().map(|p| p.as_str()).unwrap_or("/"))
        .build()
        .map_err(|_| Error::HttpRequestUriInvalid)?;

    // The connection runs as its own task; it ends once the request and
    // response bodies are done or dropped.
    let conn = tokio::spawn(conn);
    let res = tokio::time::timeout(first_byte_timeout, sender.send_request(req))
        .await
        .map_err(|_| Error::ConnectionReadTimeout)??;
    let res = res.map(|b| b.map_err(Error::from).boxed_unsync());
    let conn = async move { conn.await.map_err(|_| Error::ConnectionTerminated)?.map_err(Error::from) };
    Ok((res, Box::pin(conn)))
}

/// One TLS client config for every request: building it parses the whole
/// webpki root store.
static TLS: LazyLock<tokio_rustls::TlsConnector> = LazyLock::new(|| {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.into(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
});

trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Sync + Unpin + 'static {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Sync + Unpin + 'static> Stream for T {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_matching() {
        let p = EgressPolicy::parse(&[
            "api.example.com".into(),
            "*.cdn.example.net:443".into(),
            "127.0.0.1:9000".into(),
        ], true)
        .unwrap();
        assert!(p.matches("api.example.com", 443).is_some());
        assert!(p.matches("API.EXAMPLE.COM", 80).is_some());
        assert!(p.matches("evil.com", 443).is_none());
        assert!(p.matches("api.example.com.evil.com", 443).is_none());
        assert!(p.matches("a.cdn.example.net", 443).is_some());
        assert!(p.matches("a.cdn.example.net", 80).is_none());
        assert!(p.matches("cdn.example.net", 443).is_none());
        assert!(p.matches("127.0.0.1", 9000).is_some());
        assert!(p.matches("127.0.0.1", 9001).is_none());
    }

    #[test]
    fn ip_filter() {
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
            "64:ff9b::7f00:1",
        ] {
            assert!(!is_global(bad.parse().unwrap()), "{bad}");
        }
        for good in ["1.1.1.1", "93.184.216.34", "2606:4700:4700::1111"] {
            assert!(is_global(good.parse().unwrap()), "{good}");
        }
    }

    fn policy(entries: &[&str]) -> EgressPolicy {
        EgressPolicy::parse(&entries.iter().map(|e| e.to_string()).collect::<Vec<_>>(), true).unwrap()
    }

    /// Entries that aren't `host`, `host:port` or `*.suffix[:port]` are
    /// refused, naming the entry.
    #[test]
    fn parse_refuses_malformed_entries() {
        for bad in [
            "",
            "  ",
            "*.",
            "http://a.com",
            "a.com/path",
            "a.com?q",
            "a b.com",
            "a.com:1:2",
            "2001:db8::1",
            "[::1",
            "::1]",
        ] {
            let e = EgressPolicy::parse(&[bad.to_string()], true).unwrap_err().to_string();
            assert!(e.starts_with("bad egress entry"), "{bad:?}: {e}");
        }
        assert!(EgressPolicy::parse(&[], false).unwrap().rules.is_empty());
        assert!(policy(&[]).matches("a.com", 443).is_none(), "an empty list denies all");
    }

    /// An entry is trimmed and lowercased, and a port restricts it to that
    /// port only.
    #[test]
    fn parse_normalizes_and_reads_ports() {
        let p = policy(&["  API.Example.COM:8443 ", "plain.com", "zero.com:0"]);
        assert!(p.matches("api.example.com", 8443).is_some());
        assert!(p.matches("api.example.com", 443).is_none());
        for port in [1, 80, 443, 65535] {
            assert!(p.matches("plain.com", port).is_some());
        }
        assert_eq!(p.matches("zero.com", 0).unwrap().port, Some(0));
        assert!(p.matches("zero.com", 80).is_none());
    }

    /// A port that isn't a u16 is a malformed entry: taking it as "any port"
    /// would widen what the operator allowed.
    #[test]
    fn parse_refuses_bad_ports() {
        for bad in ["a.com:99999", "a.com:8o", "*.a.com:-1", "a.com:"] {
            assert!(EgressPolicy::parse(&[bad.to_string()], true).is_err(), "{bad:?}");
        }
    }

    /// Outside dev mode a list names no private address: the tenant wrote
    /// it. Public addresses and names (checked once resolved) are fine.
    #[test]
    fn parse_refuses_private_addresses_unless_dev() {
        for private in ["127.0.0.1:9000", "169.254.169.254", "[::1]", "10.0.0.1", "[::ffff:127.0.0.1]:80"] {
            let e = EgressPolicy::parse(&[private.to_string()], false).unwrap_err().to_string();
            assert!(e.contains("private address"), "{private}: {e}");
            EgressPolicy::parse(&[private.to_string()], true).unwrap();
        }
        let public = ["1.1.1.1:443", "[2606:4700:4700::1111]", "localhost", "*.0.0.1"].map(String::from);
        EgressPolicy::parse(&public, false).unwrap();
    }

    /// IPv6 literals are bracketed, with or without a port, and match the
    /// address however the request spells it; an IP rule matches no name
    /// and a name rule no IP.
    #[test]
    fn ipv6_entries_match_by_address() {
        let p = policy(&["[::1]:8080", "[2001:DB8::1]", "10.0.0.1"]);
        assert!(p.matches("::1", 8080).is_some());
        assert!(p.matches("0:0:0:0:0:0:0:1", 8080).is_some());
        assert!(p.matches("::1", 8081).is_none());
        assert!(p.matches("2001:db8:0::1", 1).is_some());
        assert!(p.matches("10.0.0.1", 1).is_some());
        assert!(
            p.matches("::ffff:10.0.0.1", 1).is_none(),
            "a mapped address is a different address"
        );
        assert!(matches!(p.rules[0].host, HostPat::Ip(IpAddr::V6(_))));
        let names = policy(&["localhost"]);
        assert!(names.matches("127.0.0.1", 80).is_none());
    }

    /// A wildcard matches any depth of subdomain, never the bare suffix, a
    /// name that merely ends with the same letters, or (written over an IP)
    /// an IP literal as an IP rule would; `*` alone allows nothing.
    #[test]
    fn wildcard_edges() {
        let p = policy(&["*.Example.com"]);
        assert!(p.matches("a.b.c.example.com", 1).is_some());
        assert!(p.matches("A.EXAMPLE.COM", 1).is_some());
        for no in [
            "example.com",
            "badexample.com",
            "example.com.evil",
            ".example.com.x",
            "a.example.co",
        ] {
            assert!(p.matches(no, 1).is_none(), "{no}");
        }
        assert!(matches!(policy(&["*.1.2.3.4"]).rules[0].host, HostPat::Suffix(_)));
        let star = policy(&["*"]);
        for host in ["a.com", "*.com", "1.1.1.1"] {
            assert!(star.matches(host, 443).is_none(), "{host}");
        }
    }

    /// Payload-free errors survive the trip between processes by name;
    /// anything else becomes `ConnectionTerminated`.
    #[test]
    fn errors_cross_by_name() {
        use Error::*;
        for e in [
            HttpRequestDenied,
            HttpRequestUriInvalid,
            DestinationIpProhibited,
            DestinationNotFound,
            DnsTimeout,
            ConnectionTimeout,
            ConnectionReadTimeout,
            ConnectionRefused,
        ] {
            let name = format!("{e:?}");
            assert_eq!(format!("{:?}", error_named(name.as_bytes())), name);
        }
        for other in [&b""[..], b"Connect", b"httprequestdenied", b"HttpRequestDenied ", b"ok"] {
            assert!(matches!(error_named(other), ConnectionTerminated), "{other:?}");
        }
    }

    fn get(uri: &str) -> http::Request<WasiBody> {
        let body = http_body_util::Empty::<bytes::Bytes>::new()
            .map_err(|e| match e {})
            .boxed_unsync();
        http::Request::get(uri).body(body).unwrap()
    }

    /// Requests are checked before any connection: not on the list (with
    /// the scheme's default port), no host, or an allowed name pointing at a
    /// private address, whether written as an IP or resolved.
    #[tokio::test]
    async fn send_refuses_before_connecting() {
        let p = policy(&["a.example:443", "*.0.0.1", "localhost"]);
        let refused = async |uri: &str| send(get(uri), None, &p).await.err().map(|e| format!("{e:?}"));
        assert_eq!(refused("http://a.example/").await.as_deref(), Some("HttpRequestDenied"));
        assert_eq!(
            refused("http://other.example:443/").await.as_deref(),
            Some("HttpRequestDenied")
        );
        assert_eq!(refused("/relative").await.as_deref(), Some("HttpRequestUriInvalid"));
        assert_eq!(
            refused("http://127.0.0.1/").await.as_deref(),
            Some("DestinationIpProhibited")
        );
        assert_eq!(
            refused("http://localhost/").await.as_deref(),
            Some("DestinationIpProhibited")
        );
    }
}
