//! `nanovm proxy`: the public front door. It finds which app a request's
//! host serves in DNS, names that app's current signed version to the node,
//! and forwards requests and WebSocket events.
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
use http::header::{CONNECTION, PROXY_AUTHORIZATION, TE, TRAILER, UPGRADE};
use http::{HeaderName, HeaderValue, Request, Response, StatusCode};
use http_body_util::{BodyExt, Empty, combinators::UnsyncBoxBody};
use hyper::body::Incoming;
use oci_spec::distribution::Reference;
use parking_lot::Mutex;
use tokio::sync::OnceCell;

use crate::config::ProxyConfig;
use crate::object::{RespBody, error_response};
use crate::registry::{APP_HEADER, HOST_HEADER, MIN_VERSION_HEADER, Oci, TRUST_HEADER};
use crate::ws::{self, Sockets};

/// How long a client may take over each request's headers (or idle between
/// requests).
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

/// What `_nanovm.<host>` says.
#[derive(Clone, Debug, PartialEq)]
struct Route {
    /// The name the record was found at, after CNAMEs, without `_nanovm.`:
    /// the app is this name's deployment of its repository, so aliases
    /// share one app.
    owner: String,
    /// `repository:tag`.
    app: Reference,
    trust: String,
    min: Option<String>,
}

pub struct Proxy {
    cfg: Arc<ProxyConfig>,
    http: reqwest::Client,
    registry: Oci,
    node: hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, Body>,
    routes: Cache<Option<Route>>,
    digests: Cache<String>,
    sockets: Mutex<HashMap<String, Arc<Sockets>>>,
    total: Arc<std::sync::atomic::AtomicUsize>,
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

pub(crate) type Body = UnsyncBoxBody<bytes::Bytes, hyper::Error>;

impl Proxy {
    pub fn new(cfg: ProxyConfig) -> anyhow::Result<Arc<Proxy>> {
        cfg.validate()?;
        let node = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(hyper_util::client::legacy::connect::HttpConnector::new());
        Ok(Arc::new(Proxy {
            registry: Oci::new(cfg.dev)?,
            cfg: Arc::new(cfg),
            http: reqwest::Client::new(),
            node,
            routes: Cache::default(),
            digests: Cache::default(),
            sockets: Mutex::default(),
            total: Arc::default(),
        }))
    }

    pub async fn handle(self: Arc<Self>, req: Request<Incoming>) -> Response<RespBody> {
        match self.forward(req).await {
            Ok(res) => res,
            Err((status, why)) => error_response(status.as_u16(), "proxy", &why),
        }
    }

    async fn resolve(&self, host: &str) -> Result<(String, http::HeaderMap), (StatusCode, String)> {
        let bad = |s: StatusCode| move |e: anyhow::Error| (s, format!("{e:#}"));
        let route = self.route(host).await.map_err(bad(StatusCode::BAD_GATEWAY))?;
        let route = route.ok_or((StatusCode::NOT_FOUND, format!("no app at {host}")))?;
        let digest = self.digest(&route.app).await.map_err(bad(StatusCode::BAD_GATEWAY))?;
        let mut headers = http::HeaderMap::new();
        let app = format!("{}/{}@{digest}", route.app.registry(), route.app.repository());
        for (k, v) in [(APP_HEADER, Some(app.as_str())), (HOST_HEADER, Some(route.owner.as_str())),
            (TRUST_HEADER, Some(route.trust.as_str())), (MIN_VERSION_HEADER, route.min.as_deref())] {
            if let Some(v) = v {
                headers.insert(k, HeaderValue::from_str(v).map_err(|_| (StatusCode::BAD_GATEWAY, format!("bad {k}")))?);
            }
        }
        Ok((crate::registry::app_name(&route.owner, &route.app), headers))
    }

    async fn forward(self: &Arc<Self>, req: Request<Incoming>) -> Result<Response<RespBody>, (StatusCode, String)> {
        if ws::is_event(&req) {
            return Err((StatusCode::BAD_REQUEST, "socket events come from the proxy".into()));
        }
        let host = req.headers().get(http::header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
        let host = host.rsplit_once(':').map_or(host, |(h, _)| h).to_ascii_lowercase();
        let (name, headers) = self.resolve(&host).await?;
        let upgrading = ws::wants(&req);
        let (mut parts, body) = req.into_parts();
        // Only the proxy names apps to the node.
        let theirs: Vec<_> = parts.headers.keys().filter(|k| k.as_str().starts_with("nanovm-")).cloned().collect();
        for k in theirs {
            parts.headers.remove(k);
        }
        // Hop-by-hop headers belong to the client connection, not the node.
        let h = &mut parts.headers;
        let listed: Vec<_> = h.get_all(CONNECTION).iter().flat_map(|v| v.to_str().unwrap_or("").split(',')).collect();
        let listed: Vec<HeaderName> = listed.iter().filter_map(|n| n.trim().parse().ok()).collect();
        for k in listed.iter().chain(&[CONNECTION, UPGRADE, TE, TRAILER, PROXY_AUTHORIZATION]) {
            h.remove(k);
        }
        for k in ["keep-alive", "proxy-connection"] {
            h.remove(k);
        }
        let pq = parts.uri.path_and_query().map_or("/", |p| p.as_str());
        parts.uri = format!("http://{}{pq}", self.cfg.node).parse().map_err(|_| (StatusCode::BAD_REQUEST, pq.to_string()))?;

        if upgrading {
            return Ok(self.socket(name, host, Request::from_parts(parts, body.boxed_unsync())).await);
        }
        parts.headers.extend(headers);
        let res = self
            .node
            .request(Request::from_parts(parts, body.boxed_unsync()))
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
        Ok(res.map(|b| b.map_err(Into::into).boxed_unsync()))
    }

    async fn socket(self: &Arc<Self>, name: String, host: String, req: Request<Body>) -> Response<RespBody> {
        let Some((object, _)) = crate::table::route(req.uri().path()) else {
            return error_response(400, "bad-object-id", crate::table::BAD_OBJECT);
        };
        let object = object.to_string();
        let sockets = {
            let mut apps = self.sockets.lock();
            // Pending opens, live connections and publishes each hold an Arc.
            apps.retain(|_, s| Arc::strong_count(s) > 1);
            if apps.len() >= self.cfg.max_sockets && !apps.contains_key(&name) {
                return error_response(503, "socket-refused", "the proxy has max_sockets open");
            }
            apps.entry(name.clone()).or_insert_with(|| Arc::new(Sockets::new(self.cfg.clone(), self.total.clone()))).clone()
        };
        let (proxy, max) = (self.clone(), (self.cfg.max_response_body_kb << 10) as usize);
        let deliver: ws::Deliver = Box::new(move |mut req| {
            let (proxy, host, name) = (proxy.clone(), host.clone(), name.clone());
            Box::pin(async move {
                let event = async {
                    let Ok((current, headers)) = proxy.resolve(&host).await else { return ws::Reply::Bad(502) };
                    if current != name { return ws::Reply::Bad(502) }
                    req.headers_mut().extend(headers);
                    match proxy.node.request(req).await {
                        Ok(res) => ws::answer(res, max).await,
                        Err(_) => ws::Reply::Bad(502),
                    }
                };
                tokio::time::timeout(Duration::from_secs(240), event).await.unwrap_or(ws::Reply::Bad(502))
            })
        });
        // Finish OPEN even when the client leaves, then deliver DISCONNECT.
        let open = tokio::spawn(async move { sockets.open(&object, req, deliver).await });
        match open.await.unwrap_or(Err((500, "opening the socket failed"))) {
            Ok(res) => res.map(|()| Empty::new().map_err(|e| match e {}).boxed_unsync()),
            Err((status, why)) => {
                let mut res = error_response(status, "socket-refused", why);
                res.headers_mut().insert(http::header::SEC_WEBSOCKET_VERSION, "13".parse().unwrap());
                res
            }
        }
    }

    async fn publish(&self, req: Request<Incoming>) -> Response<RespBody> {
        let sockets = req.headers().get(APP_HEADER).and_then(|h| h.to_str().ok()).and_then(|app| self.sockets.lock().get(app).cloned());
        let status = if req.method() != http::Method::POST || req.uri().path() != "/publish" { 404 }
            else if let Some(s) = sockets { s.publish(req).await }
            else { 200 };
        Response::builder().status(status).body(Empty::new().map_err(|e| match e {}).boxed_unsync()).unwrap()
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
            [a] => {
                let name = a["name"].as_str().unwrap_or_default().trim_end_matches('.').to_ascii_lowercase();
                let owner = name.strip_prefix("_nanovm.").unwrap_or(&name).to_string();
                let route = Route { owner, ..parse(&unquote(a["data"].as_str().unwrap_or_default()))? };
                (Some(route), a["TTL"].as_u64().unwrap_or(30))
            }
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
    Ok(Route { owner: String::new(), app, trust: trust.join(", "), min })
}

/// Serves the proxy on `listener`, over plain HTTP: TLS is left to a
/// terminator in front.
pub async fn serve(proxy: Arc<Proxy>, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    loop {
        let p = proxy.clone();
        crate::server::serve_conn(crate::server::accept(&listener).await, Some(CLIENT_TIMEOUT), move |req| p.clone().handle(req));
    }
}

/// Private control port: network policy must restrict it to nodes.
pub async fn serve_publish(proxy: Arc<Proxy>, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    loop {
        let p = proxy.clone();
        crate::server::serve_conn(crate::server::accept(&listener).await, Some(CLIENT_TIMEOUT), move |req| {
            let p = p.clone();
            async move { p.publish(req).await }
        });
    }
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
