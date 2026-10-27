//! Outbound HTTP for guests: a per-app host allowlist, a per-object
//! connection semaphore, and an IP filter applied to the resolved address
//! (so DNS cannot point an allowed name at loopback, link-local metadata
//! endpoints or private ranges). The connection goes to the address that was
//! checked, never re-resolved.

use std::future::{Future, poll_fn};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::task::{Poll, ready};
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
            let (host, port) = split_host_port(&e)?;
            let host = if let Some(s) = host.strip_prefix("*.") {
                HostPat::Suffix(format!(".{s}"))
            } else if let Ok(ip) = host.parse::<IpAddr>() {
                HostPat::Ip(ip)
            } else {
                HostPat::Exact(host.to_string())
            };
            rules.push(Rule { host, port });
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

fn split_host_port(s: &str) -> anyhow::Result<(&str, Option<u16>)> {
    if let Some(rest) = s.strip_prefix('[') {
        let (h, tail) = rest
            .split_once(']')
            .ok_or_else(|| anyhow::anyhow!("bad egress entry {s:?}"))?;
        let port = match tail.strip_prefix(':') {
            Some(p) => Some(p.parse()?),
            None => None,
        };
        return Ok((h, port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Ok((h, Some(p.parse()?))),
        _ => Ok((s, None)),
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
                || s[0] == 0x2002 // 6to4
                || v6 == Ipv6Addr::UNSPECIFIED)
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

pub struct EgressHooks {
    pub policy: Arc<EgressPolicy>,
    pub sem: Arc<Semaphore>,
    pub metrics: Arc<Metrics>,
    pub app: Arc<str>,
    pub object: Arc<str>,
}

type IoFuture = Box<dyn Future<Output = Result<(), Error>> + Send>;

impl WasiHttpHooks for EgressHooks {
    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
        _fut: Box<dyn Future<Output = Result<(), Error>> + Send>,
    ) -> Box<dyn Future<Output = Result<(http::Response<WasiBody>, IoFuture), Error>> + Send> {
        let policy = self.policy.clone();
        let sem = self.sem.clone();
        let metrics = self.metrics.clone();
        let (app, object) = (self.app.clone(), self.object.clone());
        Box::new(async move {
            let target = request.uri().to_string();
            let res = send(request, options, &policy, sem).await;
            match &res {
                Ok(_) => {
                    metrics.egress_allowed.fetch_add(1, Relaxed);
                }
                Err(e) => {
                    metrics.egress_denied.fetch_add(1, Relaxed);
                    metrics.event("egress-denied", &app, &object, format!("{target} → {e}"));
                }
            }
            res
        })
    }
}

async fn send(
    mut req: http::Request<WasiBody>,
    options: Option<RequestOptions>,
    policy: &EgressPolicy,
    sem: Arc<Semaphore>,
) -> Result<(http::Response<WasiBody>, IoFuture), Error> {
    let uri = req.uri().clone();
    let tls = uri.scheme() == Some(&Scheme::HTTPS);
    let host = uri.host().ok_or(Error::HttpRequestUriInvalid)?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });

    let rule = policy.matches(host, port).ok_or(Error::HttpRequestDenied)?;
    let permit = sem.try_acquire_owned().map_err(|_| Error::ConnectionLimitReached)?;

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
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.into(),
        };
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|_| Error::TlsProtocolError)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(host.to_string())?;
        let s = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .map_err(Error::Tls)?;
        Box::new(s)
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

    let send = async move {
        tokio::time::timeout(first_byte_timeout, sender.send_request(req))
            .await
            .map_err(|_| Error::ConnectionReadTimeout)?
            .map_err(Error::from)
    };
    // Drive the connection while waiting for the response head.
    let mut send = pin!(send);
    let mut conn = Some(conn);
    let res = poll_fn(|cx| match send.as_mut().poll(cx) {
        Poll::Ready(r) => Poll::Ready(r),
        Poll::Pending => {
            let Some(c) = conn.as_mut() else { return Poll::Pending };
            let r = ready!(Pin::new(c).poll(cx));
            conn = None;
            match r {
                Ok(()) => send.as_mut().poll(cx),
                Err(e) => Poll::Ready(Err(Error::from(e))),
            }
        }
    })
    .await?;

    let res = res.map(|b| b.map_err(Error::from).boxed_unsync());
    let io: IoFuture = Box::new(async move {
        let _permit = permit;
        match conn {
            Some(c) => c.await.map_err(Error::from),
            None => Ok(()),
        }
    });
    Ok((res, io))
}

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
