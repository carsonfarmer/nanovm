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
    /// An IP literal; the operator named it explicitly, so it bypasses the
    /// private-range filter.
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
    pub fn parse(entries: &[String]) -> anyhow::Result<Self> {
        let mut rules = Vec::new();
        for e in entries {
            let e = e.trim().to_ascii_lowercase();
            let (wild, rest) = e.strip_prefix("*.").map_or((false, e.as_str()), |r| (true, r));
            // `host`, `host:port` or `[v6]:port`.
            let a: http::uri::Authority = rest.parse().map_err(|_| anyhow::anyhow!("bad egress entry {e:?}"))?;
            let host = a.host().trim_start_matches('[').trim_end_matches(']');
            let host = match host.parse::<IpAddr>() {
                _ if wild => HostPat::Suffix(format!(".{host}")),
                Ok(ip) => HostPat::Ip(ip),
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

pub struct EgressHooks {
    pub route: Route,
    pub sem: Arc<Semaphore>,
    pub metrics: Arc<Metrics>,
    pub obj: Arc<crate::object::ObjShared>,
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
        // Names the object, so its publishes reach only its own sockets.
        let mut request = request;
        if request.uri().host() == Some(crate::ws::PUBLISH_HOST) {
            request.headers_mut().insert(crate::ws::OBJECT_HEADER, obj.id.parse().expect("object ids are header-safe"));
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
        ])
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
}
