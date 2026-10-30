//! `nanovm proxy`: the public front door. It finds which app a request's
//! host serves in DNS, names that app's current signed version to the node,
//! and forwards the request (WebSockets included).
//!
//! A host's app is a TXT record at `_nanovm.<host>`:
//!
//! ```text
//! app=ghcr.io/org/app:prod; trust=<issuer> <subject>; trust=…; min=1.2.0
//! ```
//!
//! naming the repository and the tag deployed, each identity whose signature
//! is accepted, and an optional version floor. Whoever controls the domain
//! controls its routing; the node runs only what one of those identities
//! signed, and keys an app's data by its repository, so a forged record can
//! never reach another app's data. A deploy moves the tag; rollback moves it
//! back. Records are cached for their TTL and tags for `tag_ttl_s`; the
//! records are asked of a DNS-over-HTTPS resolver.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use http::{HeaderValue, Request, Response, StatusCode};
use http_body_util::{BodyExt, combinators::UnsyncBoxBody};
use hyper::body::Incoming;
use oci_client::Reference;
use parking_lot::Mutex;
use tokio::sync::OnceCell;

use crate::config::ProxyConfig;
use crate::object::{RespBody, error_response};
use crate::registry::{APP_HEADER, HOST_HEADER, MIN_VERSION_HEADER, Oci, TRUST_HEADER};

/// How long a client may take over its TLS handshake, and over each
/// request's headers (or idling between requests).
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

/// What `_nanovm.<host>` says.
#[derive(Clone, Debug, PartialEq)]
struct Route {
    /// `repository:tag`.
    app: Reference,
    trust: String,
    min: Option<String>,
}

pub struct Proxy {
    cfg: ProxyConfig,
    http: reqwest::Client,
    registry: Oci,
    node: hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, Body>,
    routes: Cache<Option<Route>>,
    digests: Cache<String>,
}

/// Hosts or tags remembered at most, each cache.
const MAX_CACHED: usize = 10_000;
/// How long a failed lookup is answered again rather than retried.
const ERROR_TTL: Duration = Duration::from_secs(5);

/// Answers kept until they expire, failures included (briefly). However many
/// ask for a key at once, it is looked up once.
#[derive(Default)]
struct Cache<T>(Mutex<HashMap<String, Arc<OnceCell<(Result<T, String>, Instant)>>>>);

impl<T: Clone> Cache<T> {
    /// `key`'s answer, from `lookup` (an answer and how long it lasts) when
    /// there is none fresh.
    async fn get(&self, key: &str, lookup: impl Future<Output = anyhow::Result<(T, Duration)>>) -> anyhow::Result<T> {
        let cell = {
            let mut cells = self.0.lock();
            let now = Instant::now();
            let fresh = |c: &Arc<OnceCell<(Result<T, String>, Instant)>>| c.get().is_none_or(|(_, until)| *until > now);
            match cells.get(key).filter(|c| fresh(c)) {
                Some(c) => c.clone(),
                None => {
                    if cells.len() >= MAX_CACHED {
                        cells.retain(|_, c| fresh(c));
                        if cells.len() >= MAX_CACHED {
                            cells.clear();
                        }
                    }
                    cells.entry(key.to_string()).insert_entry(Arc::default()).get().clone()
                }
            }
        };
        let (answer, _) = cell
            .get_or_init(async || match lookup.await {
                Ok((v, ttl)) => (Ok(v), Instant::now() + ttl),
                Err(e) => (Err(format!("{e:#}")), Instant::now() + ERROR_TTL),
            })
            .await;
        answer.clone().map_err(anyhow::Error::msg)
    }
}

type Body = UnsyncBoxBody<bytes::Bytes, hyper::Error>;

impl Proxy {
    pub fn new(cfg: ProxyConfig) -> anyhow::Result<Arc<Proxy>> {
        cfg.validate()?;
        let node = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(hyper_util::client::legacy::connect::HttpConnector::new());
        Ok(Arc::new(Proxy {
            registry: Oci::new(cfg.dev)?,
            cfg,
            http: reqwest::Client::new(),
            node,
            routes: Cache::default(),
            digests: Cache::default(),
        }))
    }

    pub async fn handle(self: Arc<Self>, req: Request<Incoming>) -> Response<RespBody> {
        match self.forward(req).await {
            Ok(res) => res,
            Err((status, why)) => error_response(status.as_u16(), "proxy", &why),
        }
    }

    async fn forward(&self, req: Request<Incoming>) -> Result<Response<RespBody>, (StatusCode, String)> {
        let bad = |s: StatusCode| move |e: anyhow::Error| (s, format!("{e:#}"));
        let host = req.headers().get(http::header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
        let host = host.rsplit_once(':').map_or(host, |(h, _)| h).to_ascii_lowercase();
        let route = self.route(&host).await.map_err(bad(StatusCode::BAD_GATEWAY))?;
        let route = route.ok_or((StatusCode::NOT_FOUND, format!("no app at {host}")))?;
        let digest = self.digest(&route.app).await.map_err(bad(StatusCode::BAD_GATEWAY))?;

        let (mut parts, body) = req.into_parts();
        let upgrade = parts.extensions.remove::<hyper::upgrade::OnUpgrade>();
        // Only the proxy names apps to the node.
        let theirs: Vec<_> = parts.headers.keys().filter(|k| k.as_str().starts_with("nanovm-")).cloned().collect();
        for k in theirs {
            parts.headers.remove(k);
        }
        let app = format!("{}/{}@{digest}", route.app.registry(), route.app.repository());
        let set = |h: &mut http::HeaderMap, k: &'static str, v: &str| -> Result<(), (StatusCode, String)> {
            h.insert(k, HeaderValue::from_str(v).map_err(|_| (StatusCode::BAD_GATEWAY, format!("bad {k}")))?);
            Ok(())
        };
        set(&mut parts.headers, APP_HEADER, &app)?;
        set(&mut parts.headers, HOST_HEADER, &host)?;
        set(&mut parts.headers, TRUST_HEADER, &route.trust)?;
        if let Some(min) = &route.min {
            set(&mut parts.headers, MIN_VERSION_HEADER, min)?;
        }
        let pq = parts.uri.path_and_query().map_or("/", |p| p.as_str());
        parts.uri = format!("http://{}{pq}", self.cfg.node).parse().map_err(|_| (StatusCode::BAD_REQUEST, pq.to_string()))?;

        let mut res = self
            .node
            .request(Request::from_parts(parts, body.boxed_unsync()))
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
        // A WebSocket: once both sides have switched, bytes flow both ways.
        if res.status() == StatusCode::SWITCHING_PROTOCOLS
            && let Some(client) = upgrade
        {
            let node = hyper::upgrade::on(&mut res);
            tokio::spawn(async move {
                if let (Ok(client), Ok(node)) = (client.await, node.await) {
                    let (mut client, mut node) = (hyper_util::rt::TokioIo::new(client), hyper_util::rt::TokioIo::new(node));
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut node).await;
                }
            });
        }
        Ok(res.map(|b| b.map_err(Into::into).boxed_unsync()))
    }

    /// The route `_nanovm.<host>` names, if any, from the cache while its TTL
    /// lasts.
    async fn route(&self, host: &str) -> anyhow::Result<Option<Route>> {
        self.routes.get(host, self.lookup(host)).await
    }

    async fn lookup(&self, host: &str) -> anyhow::Result<(Option<Route>, Duration)> {
        let url = reqwest::Url::parse_with_params(&self.cfg.resolver, [("name", format!("_nanovm.{host}")), ("type", "TXT".into())])?;
        let answer: serde_json::Value =
            self.http.get(url).header("accept", "application/dns-json").send().await?.error_for_status()?.json().await?;
        let records = answer["Answer"].as_array().map(Vec::as_slice).unwrap_or_default();
        let txt: Vec<_> = records.iter().filter(|a| a["type"] == 16).collect();
        let (route, ttl) = match txt[..] {
            [] => (None, 30),
            [a] => (Some(parse(&unquote(a["data"].as_str().unwrap_or_default()))?), a["TTL"].as_u64().unwrap_or(30)),
            _ => bail!("_nanovm.{host} has more than one TXT record"),
        };
        Ok((route, Duration::from_secs(ttl.clamp(1, 3600))))
    }

    /// The digest `app`'s tag names, from the cache for `tag_ttl_s`.
    async fn digest(&self, app: &Reference) -> anyhow::Result<String> {
        let key = app.whole();
        let lookup = async {
            let digest = self.registry.digest(app).await.with_context(|| format!("resolving {key}"))?;
            Ok((digest, Duration::from_secs(self.cfg.tag_ttl_s)))
        };
        self.digests.get(&key, lookup).await
    }
}

/// A TXT record's text: its quoted strings, joined.
fn unquote(data: &str) -> String {
    data.split('"').skip(1).step_by(2).collect()
}

/// `app=<repository>:<tag>; trust=<issuer> <subject>; …; min=<version>`.
fn parse(txt: &str) -> anyhow::Result<Route> {
    let (mut app, mut trust, mut min) = (None, vec![], None);
    for field in txt.split(';').map(str::trim).filter(|f| !f.is_empty()) {
        match field.split_once('=') {
            Some(("app", a)) if app.is_none() => app = Some(a.trim().parse::<Reference>().context("app")?),
            Some(("trust", t)) => match t.split_whitespace().collect::<Vec<_>>()[..] {
                [issuer, subject] => trust.push(format!("{issuer} {subject}")),
                _ => bail!("trust is `<issuer> <subject>`"),
            },
            Some(("min", m)) if min.is_none() => min = Some(m.trim().parse::<semver::Version>()?.to_string()),
            _ => bail!("unknown or repeated field {field:?}"),
        }
    }
    let app = app.context("no app=")?;
    if app.digest().is_some() || app.tag().is_none() {
        bail!("app= names a tag, not a digest");
    }
    if trust.is_empty() {
        bail!("no trust=");
    }
    Ok(Route { app, trust: trust.join(", "), min })
}

/// Serves the proxy on `listener`, over TLS if it has a certificate.
pub async fn serve(proxy: Arc<Proxy>, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    let tls = match (&proxy.cfg.tls_cert, &proxy.cfg.tls_key) {
        (Some(cert), Some(key)) => Some(tls(cert, key)?),
        (None, None) => None,
        _ => bail!("tls_cert and tls_key go together"),
    };
    loop {
        let sock = crate::server::accept(&listener).await;
        let (p, tls) = (proxy.clone(), tls.clone());
        tokio::spawn(async move {
            let f = move |req| p.clone().handle(req);
            match tls {
                Some(tls) => {
                    if let Ok(Ok(sock)) = tokio::time::timeout(CLIENT_TIMEOUT, tls.accept(sock)).await {
                        crate::server::serve_conn(sock, Some(CLIENT_TIMEOUT), f);
                    }
                }
                None => {
                    crate::server::serve_conn(sock, Some(CLIENT_TIMEOUT), f);
                }
            }
        });
    }
}

fn tls(cert: &std::path::Path, key: &std::path::Path) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    let certs = CertificateDer::pem_file_iter(cert)?.collect::<Result<Vec<_>, _>>().context("tls_cert")?;
    let key = PrivateKeyDer::from_pem_file(key).context("tls_key")?;
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key is looked up once however many ask at once; a failure is
    /// answered again until `ERROR_TTL`; and the cache never holds more
    /// than `MAX_CACHED` keys.
    #[tokio::test(flavor = "multi_thread")]
    async fn caches_are_single_flight_and_bounded() {
        use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
        let (cache, asked) = (Arc::new(Cache::<u32>::default()), Arc::new(AtomicUsize::new(0)));
        let slow = |asked: Arc<AtomicUsize>, ok: bool| async move {
            asked.fetch_add(1, Relaxed);
            tokio::time::sleep(Duration::from_millis(50)).await;
            if ok { Ok((7, Duration::from_secs(60))) } else { Err(anyhow::anyhow!("resolver down")) }
        };
        let many: Vec<_> = (0..20)
            .map(|_| {
                let (cache, asked) = (cache.clone(), asked.clone());
                tokio::spawn(async move { cache.get("a", slow(asked, true)).await.unwrap() })
            })
            .collect();
        for one in many {
            assert_eq!(one.await.unwrap(), 7);
        }
        assert_eq!(asked.swap(0, Relaxed), 1);
        for _ in 0..3 {
            let e = cache.get("bad", slow(asked.clone(), false)).await.unwrap_err();
            assert_eq!(e.to_string(), "resolver down");
        }
        assert_eq!(asked.load(Relaxed), 1);
        for i in 0..MAX_CACHED + 10 {
            cache.get(&i.to_string(), async { Ok((1, Duration::from_secs(60))) }).await.unwrap();
        }
        assert!(cache.0.lock().len() <= MAX_CACHED);
    }

    /// A record names a tagged repository, one or more identities, and an
    /// optional floor; anything else is refused.
    #[test]
    fn records_parse_strictly() {
        let r = parse("app=ghcr.io/org/app:prod; trust=https://i s1; trust=https://i s2; min=1.2.0").unwrap();
        assert_eq!(r.app.whole(), "ghcr.io/org/app:prod");
        assert_eq!((r.trust.as_str(), r.min.as_deref()), ("https://i s1, https://i s2", Some("1.2.0")));
        for bad in [
            "trust=a b",
            "app=ghcr.io/org/app:prod",
            "app=ghcr.io/org/app@sha256:0000000000000000000000000000000000000000000000000000000000000000; trust=a b",
            "app=ghcr.io/org/app:prod; trust=a",
            "app=ghcr.io/org/app:prod; trust=a b; min=x",
            "app=ghcr.io/org/app:prod; app=ghcr.io/x/y:z; trust=a b",
            "app=ghcr.io/org/app:prod; trust=a b; extra=1",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert_eq!(unquote(r#""app=x:y; " "trust=a b""#), "app=x:y; trust=a b");
    }
}
