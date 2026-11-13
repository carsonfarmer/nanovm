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
//!
//! Each object has a home among the proxy's nodes, by rendezvous hashing
//! (`rank`). A node that doesn't hold an object's lease answers 421, naming
//! the node that does if it knows (see `lease`); the proxy tries that node,
//! or the next in the object's ranking, as it does past a node it can't
//! reach, until one answers or `failover_ms` passes. Neither kind of
//! refusal ran the request, so trying again is safe. A node that took the
//! request but hasn't answered by then may have run it: the proxy answers
//! 504 and never sends it again.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use http::header::{CONNECTION, PROXY_AUTHORIZATION, TE, TRAILER, UPGRADE};
use http::{HeaderName, HeaderValue, Request, Response, StatusCode};
use http_body_util::{BodyExt, Empty, Full, combinators::UnsyncBoxBody};
use hyper::body::Incoming;
use oci_spec::distribution::Reference;
use parking_lot::Mutex;
use tokio::sync::OnceCell;

use crate::config::ProxyConfig;
use crate::object::{RespBody, error_response};
use crate::registry::{APP_HEADER, HOST_HEADER, MIN_VERSION_HEADER, Oci, TRUST_HEADER};
use crate::ws::{self, Sockets};

use crate::server::CLIENT_TIMEOUT;

/// How long to wait after trying every node for an object before trying again.
const RETRY: Duration = Duration::from_millis(100);

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
    telemetry: Arc<crate::telemetry::Telemetry>,
    http: reqwest::Client,
    registry: Oci,
    node: hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, Body>,
    routes: Cache<Option<Route>>,
    digests: Cache<String>,
    sockets: Mutex<HashMap<String, Arc<Sockets>>>,
    total: Arc<std::sync::atomic::AtomicUsize>,
    gate: Arc<crate::gate::Gate>,
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

pub(crate) type Body = UnsyncBoxBody<bytes::Bytes, crate::object::BoxError>;

/// How long the proxy waits for a node to accept a connection.
pub const CONNECT: Duration = Duration::from_secs(1);

impl Proxy {
    pub fn new(cfg: ProxyConfig) -> anyhow::Result<Arc<Proxy>> {
        cfg.validate()?;
        let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
        // A node that doesn't answer is passed over like one that refuses.
        connector.set_connect_timeout(Some(CONNECT));
        let node = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);
        let gate = crate::gate::Gate::new(&cfg.clients)?;
        Ok(Arc::new(Proxy {
            gate,
            telemetry: Arc::new(crate::telemetry::Telemetry::new(&cfg.telemetry_endpoint, "_proxy", env!("CARGO_PKG_VERSION"), None)?),
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
        self.telemetry.forward("proxy.request", req, |req| async {
            match self.forward(req).await {
                Ok(res) => res,
                Err((status, why)) => error_response(status.as_u16(), "proxy", &why),
            }
        }).await
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
        let client = self.gate.client(&req);
        if let Err(refused) = self.gate.admit(client, None) {
            return Ok(refused);
        }
        let host = req.headers().get(http::header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
        let host = host.rsplit_once(':').map_or(host, |(h, _)| h).to_ascii_lowercase();
        let (name, headers) = self.resolve(&host).await?;
        if let Err(refused) = self.gate.admit(client, Some(&name)) {
            return Ok(refused);
        }
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
        let Some((object, _)) = crate::table::route(parts.uri.path()) else {
            return Err((StatusCode::BAD_REQUEST, crate::table::BAD_OBJECT.into()));
        };
        let object = object.to_owned();
        if upgrading {
            return Ok(self.socket(name, host, object, Request::from_parts(parts, body.map_err(Into::into).boxed_unsync())).await);
        }
        parts.headers.extend(headers);
        // The node cuts a slow body too; this holds even if it doesn't (N-505).
        let req = Request::from_parts(parts, crate::server::Paced::new(body).boxed_unsync());
        let res = match self.send(&name, &object, req).await {
            Ok(res) => res,
            Err((StatusCode::REQUEST_TIMEOUT, _)) => return Ok(crate::server::body_timeout()),
            Err(e) => return Err(e),
        };
        Ok(res.map(|b| b.map_err(Into::into).boxed_unsync()))
    }

    /// Sends `req`, for `object` of app `name`, to the node holding the
    /// object: the first in its ranking that answers, or a node one of them
    /// names, until `failover_ms` passes.
    async fn send(&self, name: &str, object: &str, mut req: Request<Body>) -> Result<Response<Incoming>, (StatusCode, String)> {
        let pq = req.uri().path_and_query().map_or("/", |p| p.as_str()).to_owned();
        let uri = |node: &str| format!("http://{node}{pq}").parse::<http::Uri>().map_err(|_| (StatusCode::BAD_REQUEST, pq.clone()));
        // A body that ran out of time (N-505) is the client's fault: 408.
        let bad_gateway = |e: hyper_util::client::legacy::Error| match crate::server::timed_out(&e) {
            true => (StatusCode::REQUEST_TIMEOUT, e.to_string()),
            false => (StatusCode::BAD_GATEWAY, e.to_string()),
        };
        // One node has nowhere else to send it: stream it there.
        if let [node] = &self.cfg.nodes[..] {
            *req.uri_mut() = uri(node)?;
            return self.node.request(req).await.map_err(bad_gateway);
        }
        let (parts, mut body) = req.into_parts();
        let max = (self.cfg.max_request_body_kb << 10) as usize;
        // Buffered to retry on another node (N-182); a body that runs out of
        // time is the client's (408, N-505).
        let mut buf = bytes::BytesMut::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| match crate::server::timed_out(&*e) {
                true => (StatusCode::REQUEST_TIMEOUT, "request body".to_string()),
                false => (StatusCode::BAD_REQUEST, e.to_string()),
            })?;
            if let Ok(data) = frame.into_data() {
                if buf.len() + data.len() > max {
                    return Err((StatusCode::PAYLOAD_TOO_LARGE, "request body".into()));
                }
                buf.extend_from_slice(&data);
            }
        }
        let body = buf.freeze();
        let ranked = rank(&self.cfg.nodes, name, object);
        let deadline = Instant::now() + Duration::from_millis(self.cfg.failover_ms);
        let (mut tries, mut owner) = (0, None);
        loop {
            let node = owner.take().unwrap_or_else(|| {
                tries += 1;
                ranked[(tries - 1) % ranked.len()]
            });
            let mut req = Request::new(Full::new(body.clone()).map_err(|e| match e {}).boxed_unsync());
            (*req.method_mut(), *req.uri_mut(), *req.headers_mut()) = (parts.method.clone(), uri(node)?, parts.headers.clone());
            // A node that took the request may have run it: past the
            // deadline its outcome is unknown, and it is never sent again.
            let Ok(sent) = tokio::time::timeout_at(deadline.into(), self.node.request(req)).await else {
                let why = format!("no answer from {node} for {object} within failover_ms: the request may have run there");
                return Err((StatusCode::GATEWAY_TIMEOUT, why));
            };
            match sent {
                Ok(res) if res.status() != StatusCode::MISDIRECTED_REQUEST || !res.headers().contains_key(crate::lease::OWNER) => return Ok(res),
                Ok(res) => {
                    let named = res.headers().get(crate::lease::OWNER).and_then(|o| o.to_str().ok());
                    owner = ranked.iter().copied().find(|n| Some(*n) == named && *n != node);
                }
                Err(e) if e.is_connect() => {}
                Err(e) => return Err(bad_gateway(e)),
            }
            if Instant::now() >= deadline {
                return Err((StatusCode::SERVICE_UNAVAILABLE, format!("no node took {object}")));
            }
            if owner.is_none() && tries.is_multiple_of(ranked.len()) {
                tokio::time::sleep(RETRY).await;
            }
        }
    }

    async fn socket(self: &Arc<Self>, name: String, host: String, object: String, req: Request<Body>) -> Response<RespBody> {
        let sockets = {
            let mut apps = self.sockets.lock();
            // Pending opens, live connections and publishes each hold an Arc.
            apps.retain(|_, s| Arc::strong_count(s) > 1);
            if apps.len() >= self.cfg.max_sockets && !apps.contains_key(&name) {
                return error_response(503, "socket-refused", "the proxy has max_sockets open");
            }
            apps.entry(name.clone()).or_insert_with(|| Arc::new(Sockets::new(self.cfg.clone(), self.total.clone()))).clone()
        };
        let (proxy, max, target) = (self.clone(), (self.cfg.max_response_body_kb << 10) as usize, object.clone());
        let deliver: ws::Deliver = Box::new(move |mut req| {
            let (proxy, host, name, object) = (proxy.clone(), host.clone(), name.clone(), target.clone());
            Box::pin(async move {
                let event = async {
                    let Ok((current, headers)) = proxy.resolve(&host).await else { return ws::Reply::Bad(502) };
                    if current != name { return ws::Reply::Bad(502) }
                    req.headers_mut().extend(headers);
                    match proxy.send(&name, &object, req).await {
                        Ok(res) => ws::answer(res, max).await,
                        Err(_) => ws::Reply::Bad(502),
                    }
                };
                tokio::time::timeout(Duration::from_secs(240), event).await.unwrap_or(ws::Reply::Bad(502))
            })
        });
        sockets.upgrade(object, req, deliver).await
    }

    async fn publish(self: Arc<Self>, req: Request<Incoming>) -> Response<RespBody> {
        if req.uri().path().starts_with(crate::egress::CALLS) {
            return self.call(req).await.unwrap_or_else(|(status, why)| error_response(status.as_u16(), "proxy", &why));
        }
        let sockets = req.headers().get(APP_HEADER).and_then(|h| h.to_str().ok()).and_then(|app| self.sockets.lock().get(app).cloned());
        let status = if req.method() != http::Method::POST || req.uri().path() != "/publish" { 404 }
            else if let Some(s) = sockets { s.publish(req).await }
            else { 200 };
        Response::builder().status(status).body(Empty::new().map_err(|e| match e {}).boxed_unsync()).unwrap()
    }

    /// A node's call to an object of its app (see `supervisor::call`), sent on
    /// to the object under the version its host names now, which must still
    /// be the caller's app: calls never cross apps.
    async fn call(&self, req: Request<Incoming>) -> Result<Response<RespBody>, (StatusCode, String)> {
        let (mut parts, body) = req.into_parts();
        let named = |k| parts.headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
        let (caller, host) = (named(APP_HEADER), named(HOST_HEADER));
        let (name, headers) = self.resolve(&host).await?;
        if name != caller {
            return Err((StatusCode::BAD_GATEWAY, format!("{host} serves another app now")));
        }
        // The object is the path's; the query goes on with it.
        let path = parts.uri.path().strip_prefix("/objects").unwrap_or_default().to_owned();
        let Some((object, _)) = crate::table::route(&path) else {
            return Err((StatusCode::BAD_REQUEST, crate::table::BAD_OBJECT.into()));
        };
        let pq = parts.uri.query().map_or_else(|| path.clone(), |q| format!("{path}?{q}"));
        let theirs: Vec<_> = parts.headers.keys().filter(|k| k.as_str().starts_with("nanovm-") && *k != crate::egress::CALLERS).cloned().collect();
        for k in theirs {
            parts.headers.remove(k);
        }
        parts.headers.extend(headers);
        parts.uri = pq.parse().map_err(|_| (StatusCode::BAD_REQUEST, pq.clone()))?;
        let res = self.send(&name, object, Request::from_parts(parts, body.map_err(Into::into).boxed_unsync())).await?;
        Ok(res.map(|b| b.map_err(Into::into).boxed_unsync()))
    }

    /// The route `_nanovm.<host>` names, if any, from the cache while its TTL
    /// lasts.
    async fn route(&self, host: &str) -> anyhow::Result<Option<Route>> {
        let deadline = tokio::time::Instant::now() + CLIENT_TIMEOUT;
        let lookup = async {
            tokio::time::timeout_at(deadline, self.lookup(host)).await.context("DNS lookup deadline exceeded")?
        };
        // A cancelled single-flight initializer must not restart a waiter's budget.
        tokio::time::timeout_at(deadline, self.routes.get(host, lookup)).await.context("DNS route deadline exceeded")?
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

/// `nodes` in the order an object tries them, its home first: by rendezvous
/// hashing, each node's score for `(app, object)`, highest first. Adding or
/// removing a node moves only the objects that it then gains or loses.
pub fn rank<'a>(nodes: &'a [String], app: &str, object: &str) -> Vec<&'a str> {
    use sha2::{Digest, Sha256};
    let score = |n: &str| Sha256::new().chain_update(n).chain_update([0]).chain_update(app).chain_update([0]).chain_update(object).finalize();
    let mut ranked: Vec<_> = nodes.iter().map(|n| (score(n), n.as_str())).collect();
    ranked.sort_unstable_by(|a, b| b.cmp(a));
    ranked.into_iter().map(|(_, n)| n).collect()
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
            // The node reads them as one comma-separated header.
            Some(("trust", t)) => match t.split_whitespace().collect::<Vec<_>>()[..] {
                [issuer, subject] if !t.contains(',') => trust.push(format!("{issuer} {subject}")),
                _ => bail!("trust is `<issuer> <subject>`, without commas"),
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
    crate::gate::serve(proxy.gate.clone(), listener, Some(CLIENT_TIMEOUT), move |req| proxy.clone().handle(req)).await
}

/// Private control port: network policy must restrict it to nodes.
pub async fn serve_publish(proxy: Arc<Proxy>, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    loop {
        let p = proxy.clone();
        crate::server::serve_conn(crate::server::accept(&listener).await, Some(CLIENT_TIMEOUT), move |req| p.clone().publish(req));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Neither missing headers nor a stalled JSON body may hold initial
    /// routing forever. A timeout is cached like any other resolver error.
    #[tokio::test]
    async fn stalled_dns_responses_have_a_total_deadline() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for headers in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy = Proxy::new(ProxyConfig {
                resolver: format!("http://{}/dns-query", listener.local_addr().unwrap()),
                ..Default::default()
            }).unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                assert!(socket.read(&mut request).await.unwrap() > 0);
                if headers {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/dns-json\r\nContent-Length: 100\r\n\r\n{").await.unwrap();
                }
                std::future::pending::<()>().await;
            });
            let result = tokio::time::timeout(CLIENT_TIMEOUT + Duration::from_secs(2), proxy.resolve("stalled.example")).await;
            server.abort();
            let (status, error) = result.expect("initial DNS routing hangs").unwrap_err();
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            assert!(error.contains("deadline"), "{error}");
            let cached = tokio::time::timeout(Duration::from_millis(100), proxy.resolve("stalled.example")).await.unwrap();
            assert_eq!(cached.unwrap_err(), (status, error));
        }
    }

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

    /// With several nodes, a node that takes a request and never answers
    /// holds it no longer than `failover_ms`: the proxy answers 504 (the
    /// request may have run there) and never sends it to another node.
    #[tokio::test]
    async fn review_failover_deadline_bounds_a_connected_silent_node() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        use tokio::io::AsyncReadExt;
        let (silent, other) = (tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(), tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap());
        let nodes = vec![silent.local_addr().unwrap().to_string(), other.local_addr().unwrap().to_string()];
        tokio::spawn(async move {
            let mut held = vec![];
            loop {
                let (mut socket, _) = silent.accept().await.unwrap();
                let _ = socket.read(&mut [0; 4096]).await;
                held.push(socket);
            }
        });
        let reached = Arc::new(AtomicUsize::new(0));
        let r = reached.clone();
        tokio::spawn(async move {
            while other.accept().await.is_ok() {
                r.fetch_add(1, SeqCst);
            }
        });
        let proxy = Proxy::new(ProxyConfig { nodes: nodes.clone(), failover_ms: 1500, ..Default::default() }).unwrap();
        let object = (0..).map(|i| format!("o{i}")).find(|o| rank(&nodes, "app", o)[0] == nodes[0]).unwrap();
        let req = Request::post(format!("/{object}/charge")).body(Full::new(bytes::Bytes::from("once")).map_err(|e| match e {}).boxed_unsync());
        let t0 = Instant::now();
        let sent = tokio::time::timeout(Duration::from_secs(5), proxy.send("app", &object, req.unwrap())).await;
        let (status, why) = sent.expect("still waiting long past failover_ms").unwrap_err();
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{why}");
        assert!(t0.elapsed() < Duration::from_millis(2500), "{:?}", t0.elapsed());
        assert_eq!(reached.load(SeqCst), 0, "sent again to another node");
    }

    /// Every object ranks every node once, the homes spread evenly whatever
    /// the list's order, and removing a node moves only the objects it was
    /// home to.
    #[test]
    fn rendezvous_moves_only_what_it_must() {
        let nodes: Vec<String> = (0..4).map(|i| format!("10.0.0.{i}:8080")).collect();
        let homes = |nodes: &[String]| (0..4000).map(|o| rank(nodes, "app", &format!("o{o}"))[0].to_owned()).collect::<Vec<_>>();
        let all = homes(&nodes);
        assert_eq!(rank(&nodes, "app", "o1").len(), 4);
        assert_eq!(rank(&nodes, "app", "o1"), rank(&nodes, "app", "o1"));
        for n in &nodes {
            let share = all.iter().filter(|h| *h == n).count();
            assert!((800..1200).contains(&share), "{n}: {share}");
        }
        let three = homes(&nodes[..3]);
        for (before, after) in all.iter().zip(&three) {
            assert!(before == after || *before == nodes[3], "{before} -> {after}");
        }
        let shuffled: Vec<String> = [3, 1, 0, 2].iter().map(|&i| nodes[i].clone()).collect();
        assert_eq!(homes(&shuffled), all, "the list's order doesn't matter");
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
            // The node splits trust on commas, and would refuse every request.
            "app=ghcr.io/org/app:prod; trust=https://token,actions.githu mbruepo",
            "app=ghcr.io/org/app:prod; trust=https://i repo:o/a:ref:refs/heads/x,y",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert_eq!(unquote(r#""app=x:y; " "trust=a b""#), "app=x:y; trust=a b");
    }

    /// A record the proxy accepts names a policy the node accepts: the node
    /// reads `nanovm-trust` as comma-separated `<issuer> <subject>` pairs, so
    /// a comma inside one (found by fuzzing, D-45) made every request to the
    /// host fail at the node instead of the record being refused.
    #[test]
    fn accepted_records_are_policies_the_node_accepts() {
        use crate::registry::{APP_HEADER, AppRef, TRUST_HEADER};
        for record in [
            "app=ghcr.io/org/app:prod; trust=https://i s1; trust=https://i s2",
            "app=ghcr.io/org/app:prod; trust=https://token,actions.githu mbruepo",
            "app=ghcr.io/org/app:prod; trust=https://i repo:o/a:ref:refs/heads/x,y",
            "app=ghcr.io/org/app:prod; trust=, s",
        ] {
            let Ok(route) = parse(record) else { continue };
            let mut headers = http::HeaderMap::new();
            let app = format!("{}/{}@sha256:{}", route.app.registry(), route.app.repository(), "0".repeat(64));
            headers.insert(APP_HEADER, app.parse().unwrap());
            headers.insert(TRUST_HEADER, route.trust.parse().unwrap());
            let r = AppRef::from_headers(&headers);
            assert!(r.as_ref().is_ok_and(Option::is_some), "{record}: {:?}", r.err());
        }
    }
}
