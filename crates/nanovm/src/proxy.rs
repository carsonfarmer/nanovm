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

use anyhow::{Context, bail, ensure};
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
    /// share one app. Its record lists them (`alias=`): no other name may
    /// serve its app and data.
    owner: String,
    /// `repository:tag`.
    app: Reference,
    /// `<issuer> <subject>, …`
    trust: String,
    aliases: Vec<String>,
    min: Option<String>,
}

/// A host's route, or why it has none here.
type Answer = Result<Route, (StatusCode, String)>;

pub struct Proxy {
    cfg: Arc<ProxyConfig>,
    telemetry: Arc<crate::telemetry::Telemetry>,
    http: reqwest::Client,
    registry: Oci,
    node: hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, Body>,
    routes: Cache<Answer>,
    digests: Cache<String>,
    sockets: Mutex<HashMap<String, Arc<Sockets>>>,
    total: Arc<std::sync::atomic::AtomicUsize>,
}

/// Hosts or tags with an answer (an app, a digest) remembered at most.
const MAX_CACHED: usize = 10_000;
/// Hosts or tags without one (no record, a refusal, a failure, a lookup
/// under way) remembered at most. While that many are fresh, a new one is
/// refused (503) rather than looked up.
const MAX_UNKNOWN: usize = 1000;
/// How long a failed lookup is answered again rather than retried.
const ERROR_TTL: Duration = Duration::from_secs(5);

/// Answers kept until they expire, failures briefly. However many ask for a
/// key at once, it is looked up once. Answers `found` accepts are kept apart
/// from the rest, so made-up names neither push them out nor cost more than
/// `MAX_UNKNOWN` lookups while the rest are fresh.
struct Cache<T> {
    found: fn(&T) -> bool,
    /// The keys with a found answer, and the rest.
    cells: Mutex<[HashMap<String, Cell<T>>; 2]>,
}

/// An answer or a failure, and until when it holds.
type Cell<T> = Arc<OnceCell<(Result<T, String>, Instant)>>;

impl<T: Clone> Cache<T> {
    fn new(found: fn(&T) -> bool) -> Cache<T> {
        Cache { found, cells: Mutex::default() }
    }

    /// `key`'s answer, from `lookup` (an answer and how long it lasts) when
    /// there is none fresh.
    async fn get(&self, key: &str, lookup: impl Future<Output = anyhow::Result<(T, Duration)>>) -> Result<T, (StatusCode, String)> {
        let now = Instant::now();
        let fresh = |c: &Cell<T>| c.get().is_none_or(|(_, until)| *until > now);
        let (cell, unknown) = {
            let mut cells = self.cells.lock();
            let [found, rest] = &mut *cells;
            if let Some(c) = found.get(key).filter(|c| fresh(c)) {
                (c.clone(), false)
            } else if let Some(c) = rest.get(key).filter(|c| fresh(c)) {
                (c.clone(), true)
            } else if let Some(old) = found.get_mut(key) {
                // Looked up again where it is.
                *old = Cell::default();
                (old.clone(), false)
            } else {
                if rest.len() >= MAX_UNKNOWN {
                    rest.retain(|_, c| fresh(c));
                }
                if rest.len() >= MAX_UNKNOWN {
                    return Err((StatusCode::SERVICE_UNAVAILABLE, "too many names without an answer are remembered: try again soon".into()));
                }
                (rest.entry(key.to_string()).insert_entry(Cell::default()).get().clone(), true)
            }
        };
        let (answer, _) = cell
            .get_or_init(async || match lookup.await {
                Ok((v, ttl)) => (Ok(v), Instant::now() + ttl),
                Err(e) => (Err(format!("{e:#}")), Instant::now() + ERROR_TTL),
            })
            .await;
        // A new key's found answer moves to the found ones.
        if unknown && answer.as_ref().is_ok_and(self.found) {
            let mut cells = self.cells.lock();
            let [found, rest] = &mut *cells;
            if rest.get(key).is_some_and(|c| Arc::ptr_eq(c, &cell)) {
                rest.remove(key);
                if found.len() >= MAX_CACHED {
                    found.retain(|_, c| fresh(c));
                }
                if let Some(any) = found.keys().next().filter(|_| found.len() >= MAX_CACHED).cloned() {
                    found.remove(&any);
                }
                found.insert(key.to_string(), cell.clone());
            }
        }
        answer.clone().map_err(|e| (StatusCode::BAD_GATEWAY, e))
    }
}

pub(crate) type Body = UnsyncBoxBody<bytes::Bytes, crate::object::BoxError>;

impl Proxy {
    pub fn new(cfg: ProxyConfig) -> anyhow::Result<Arc<Proxy>> {
        cfg.validate()?;
        let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
        // A node that doesn't answer is passed over like one that refuses.
        connector.set_connect_timeout(Some(Duration::from_secs(1)));
        let node = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);
        Ok(Arc::new(Proxy {
            telemetry: Arc::new(crate::telemetry::Telemetry::new(&cfg.telemetry_endpoint, "_proxy", env!("CARGO_PKG_VERSION"), None)?),
            registry: Oci::new(cfg.dev)?,
            cfg: Arc::new(cfg),
            http: reqwest::Client::new(),
            node,
            routes: Cache::new(Result::is_ok),
            digests: Cache::new(|_| true),
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
        let route = self.route(host).await?;
        let digest = self.digest(&route.app).await?;
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
        if !self.cfg.hosts.iter().any(|h| h.strip_prefix('*').map_or(*h == host, |under| host.ends_with(under))) {
            return Err((StatusCode::FORBIDDEN, format!("this proxy doesn't serve {host}")));
        }
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

    /// The route `_nanovm.<host>` names, from the cache while its TTL lasts.
    async fn route(&self, host: &str) -> Answer {
        let deadline = tokio::time::Instant::now() + CLIENT_TIMEOUT;
        let lookup = async {
            tokio::time::timeout_at(deadline, self.lookup(host)).await.context("DNS lookup deadline exceeded")?
        };
        // A cancelled single-flight initializer must not restart a waiter's budget.
        let late = || (StatusCode::BAD_GATEWAY, "DNS route deadline exceeded".to_string());
        tokio::time::timeout_at(deadline, self.routes.get(host, lookup)).await.map_err(|_| late())??
    }

    /// `_nanovm.<host>`'s route on this proxy, and how long it lasts.
    async fn lookup(&self, host: &str) -> anyhow::Result<(Answer, Duration)> {
        let url = reqwest::Url::parse_with_params(&self.cfg.resolver, [("name", format!("_nanovm.{host}")), ("type", "TXT".into())])?;
        let answer: serde_json::Value =
            self.http.get(url).header("accept", "application/dns-json").send().await?.error_for_status()?.json().await?;
        // Only NOERROR and NXDOMAIN answer; SERVFAIL and the rest are failures.
        ensure!(matches!(answer["Status"].as_u64(), Some(0 | 3)), "the resolver answered DNS status {}", answer["Status"]);
        let records = answer["Answer"].as_array().map(Vec::as_slice).unwrap_or_default();
        let txt: Vec<_> = records.iter().filter(|a| a["type"] == 16).collect();
        let (route, ttl) = match txt[..] {
            [] => (Err((StatusCode::NOT_FOUND, format!("no app at {host}"))), 30),
            [a] => {
                let name = a["name"].as_str().unwrap_or_default().trim_end_matches('.').to_ascii_lowercase();
                let owner = name.strip_prefix("_nanovm.").unwrap_or(&name).to_string();
                let mut route = Route { owner, ..parse(&unquote(a["data"].as_str().unwrap_or_default()))? };
                let signs = |t: &&str| self.cfg.signers.iter().any(|s| s.strip_suffix('*').map_or(s == t, |p| t.starts_with(p)));
                route.trust = route.trust.split(", ").filter(signs).collect::<Vec<_>>().join(", ");
                let refused = |why| Err((StatusCode::FORBIDDEN, why));
                let route = if route.owner != host && !route.aliases.iter().any(|a| a == host) {
                    refused(format!("{host} is an alias of {}, whose record doesn't list it", route.owner))
                } else if route.trust.is_empty() {
                    refused(format!("{}'s record trusts no identity that may deploy here", route.owner))
                } else {
                    Ok(route)
                };
                (route, a["TTL"].as_u64().unwrap_or(30))
            }
            _ => bail!("_nanovm.{host} has more than one TXT record"),
        };
        Ok((route, Duration::from_secs(ttl.clamp(1, 3600))))
    }

    /// The digest `app`'s tag names, from the cache for `tag_ttl_s`.
    async fn digest(&self, app: &Reference) -> Result<String, (StatusCode, String)> {
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

/// `app=<repository>:<tag>; trust=<issuer> <subject>; …; alias=<host>; …;
/// min=<version>`.
fn parse(txt: &str) -> anyhow::Result<Route> {
    let (mut app, mut trust, mut aliases, mut min) = (None, vec![], vec![], None);
    for field in txt.split(';').map(str::trim).filter(|f| !f.is_empty()) {
        match field.split_once('=') {
            Some(("app", a)) if app.is_none() => app = Some(a.trim().parse::<Reference>().context("app")?),
            // The node reads them as one comma-separated header.
            Some(("trust", t)) => match t.split_whitespace().collect::<Vec<_>>()[..] {
                [issuer, subject] if !t.contains(',') => trust.push(format!("{issuer} {subject}")),
                _ => bail!("trust is `<issuer> <subject>`, without commas"),
            },
            Some(("alias", a)) => aliases.push(a.trim().to_ascii_lowercase()),
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
    Ok(Route { owner: String::new(), app, trust: trust.join(", "), aliases, min })
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
        let (cache, asked) = (Arc::new(Cache::<u32>::new(|_| true)), Arc::new(AtomicUsize::new(0)));
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
            assert_eq!(e, (StatusCode::BAD_GATEWAY, "resolver down".into()));
        }
        assert_eq!(asked.load(Relaxed), 1);
        for i in 0..MAX_CACHED + 10 {
            cache.get(&i.to_string(), async { Ok((1, Duration::from_secs(60))) }).await.unwrap();
        }
        assert!(cache.cells.lock()[0].len() <= MAX_CACHED);
    }

    /// A DNS-over-HTTPS stub answering TXT queries for `_nanovm.<host>` from
    /// `records` by host, and counting them. A record `CNAME <name>` answers
    /// as resolvers do (the alias, then the target's records under its
    /// name); `STATUS <n>` answers DNS status `n` and nothing else.
    async fn resolver(records: &[(&str, &str)]) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::Ordering::SeqCst;
        let records: Arc<HashMap<String, String>> = Arc::new(records.iter().map(|(h, r)| (format!("_nanovm.{h}"), r.to_string())).collect());
        let (listener, asked) = (tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(), Arc::<std::sync::atomic::AtomicUsize>::default());
        let url = format!("http://{}/dns-query", listener.local_addr().unwrap());
        let counted = asked.clone();
        tokio::spawn(crate::server::serve_http1(listener, move |req: Request<Incoming>| {
            counted.fetch_add(1, SeqCst);
            let mut name = req.uri().query().unwrap_or("").split('&').find_map(|kv| kv.strip_prefix("name=")).unwrap_or("").to_string();
            let (mut answer, mut status) = (vec![], 0);
            if let Some(target) = records.get(&name).and_then(|r| r.strip_prefix("CNAME ")) {
                answer.push(serde_json::json!({ "name": name, "type": 5, "TTL": 3600, "data": format!("{target}.") }));
                name = target.to_string();
            }
            match records.get(&name) {
                Some(r) if r.starts_with("STATUS ") => status = r[7..].parse().unwrap(),
                Some(txt) => answer.push(serde_json::json!({ "name": format!("{name}."), "type": 16, "TTL": 3600, "data": format!("\"{txt}\"") })),
                None => {}
            }
            async move { crate::server::ok_json(serde_json::json!({ "Status": status, "Answer": answer })) }
        }));
        (url, asked)
    }

    /// A proxy asking `resolver`, serving `hosts` to apps `signers` sign.
    fn proxy(resolver: &str, hosts: &[&str], signers: &[&str]) -> Arc<Proxy> {
        let list = |l: &[&str]| l.iter().map(|s| s.to_string()).collect();
        Proxy::new(ProxyConfig { resolver: resolver.into(), hosts: list(hosts), signers: list(signers), ..Default::default() }).unwrap()
    }

    /// A host the operator doesn't list is refused (403) before its record
    /// is looked up, however it is spelled; with no list, every host is.
    #[tokio::test]
    async fn unlisted_hosts_are_refused_before_any_lookup() {
        use std::sync::atomic::Ordering::SeqCst;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (url, asked) = resolver(&[]).await;
        let status = async |addr: std::net::SocketAddr, host: &str| {
            let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
            c.write_all(format!("GET /a/ HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            let mut out = String::new();
            c.read_to_string(&mut out).await.unwrap();
            out[9..12].parse::<u16>().unwrap()
        };
        let refused = ["evil.test", "apps.test", "xapps.test", "app.test.evil", "a.apps.test.evil", "pp.test", "127.0.0.1"];
        for (hosts, served) in [(&[][..], &[][..]), (&["app.test", "*.apps.test"], &["app.test", "APP.Test:8443", "a.apps.test", "a.b.apps.test"])] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(serve(proxy(&url, hosts, &["i s"]), listener));
            for host in refused.iter().chain(if hosts.is_empty() { &["app.test", "a.apps.test"][..] } else { &[] }) {
                assert_eq!(status(addr, host).await, 403, "{host}");
            }
            assert_eq!(asked.load(SeqCst), 0, "looked up a host it doesn't serve");
            for host in served {
                assert_eq!(status(addr, host).await, 404, "{host} has no record");
            }
        }
        assert_eq!(asked.load(SeqCst), 3, "app.test, a.apps.test and a.b.apps.test, once each");
    }

    /// A record's identities are cut to the operator's signers, each exact
    /// or up to a final `*`; a record left with none is refused (403), as is
    /// every record on a proxy with no signers.
    #[tokio::test]
    async fn records_trust_only_the_operators_signers() {
        let mixed = "app=ghcr.io/org/app:prod; trust=https://ci repo:org/a; trust=https://ci repo:other/a; \
            trust=https://evil repo:org/a; trust=https://ci repo:org; trust=https://ci.evil repo:org/a; trust=https://exact whom";
        let (url, _) = resolver(&[("mixed.test", mixed), ("theirs.test", "app=ghcr.io/org/app:prod; trust=https://ci repo:other/a")]).await;
        let p = proxy(&url, &[], &["https://ci repo:org/*", "https://exact who"]);
        assert_eq!(p.route("mixed.test").await.unwrap().trust, "https://ci repo:org/a");
        let (status, why) = p.route("theirs.test").await.unwrap_err();
        assert!(status == StatusCode::FORBIDDEN && why.contains("trusts no identity"), "{why}");
        assert_eq!(proxy(&url, &[], &[]).route("mixed.test").await.unwrap_err().0, StatusCode::FORBIDDEN);
    }

    /// A name whose record is a CNAME of another's serves that name's app
    /// only if its record lists it (`alias=`): otherwise any domain could
    /// serve another's app, with its data, under its own origin.
    #[tokio::test]
    async fn aliases_need_the_owners_consent() {
        let (url, _) = resolver(&[
            ("a.test", "app=ghcr.io/org/app:prod; trust=i s; alias=www.a.test"),
            ("www.a.test", "CNAME _nanovm.a.test"),
            ("evil.test", "CNAME _nanovm.a.test"),
            ("b.test", "app=ghcr.io/org/app:prod; trust=i s"),
            ("www.b.test", "CNAME _nanovm.b.test"),
        ]).await;
        let p = proxy(&url, &[], &["i s"]);
        assert_eq!(p.route("a.test").await.unwrap().owner, "a.test");
        assert_eq!(p.route("www.a.test").await.unwrap().owner, "a.test");
        for host in ["evil.test", "www.b.test"] {
            let (status, why) = p.route(host).await.unwrap_err();
            assert!(status == StatusCode::FORBIDDEN && why.contains("doesn't list it"), "{host}: {why}");
        }
    }

    /// Made-up names under a served suffix cost at most `MAX_UNKNOWN`
    /// lookups while those are remembered, however many come, and push out
    /// no route that has an app. Before, each cost a lookup, and 10,000 of
    /// them emptied the cache.
    #[tokio::test(flavor = "multi_thread")]
    async fn made_up_names_cost_bounded_lookups_and_keep_live_routes() {
        use std::sync::atomic::Ordering::SeqCst;
        let (url, asked) = resolver(&[("live.apps.test", "app=ghcr.io/org/app:prod; trust=i s")]).await;
        let p = proxy(&url, &["*.apps.test"], &["i s"]);
        p.route("live.apps.test").await.unwrap();
        let (flood, mut refused) = (3 * MAX_CACHED, 0);
        for batch in (0..flood).step_by(500) {
            let many: Vec<_> = (batch..batch + 500).map(|i| {
                let p = p.clone();
                tokio::spawn(async move { p.route(&format!("r{i}.apps.test")).await.unwrap_err().0 })
            }).collect();
            for one in many {
                refused += (one.await.unwrap() == StatusCode::SERVICE_UNAVAILABLE) as usize;
            }
        }
        let n = asked.load(SeqCst);
        assert!(n <= 1 + MAX_UNKNOWN, "{n} lookups for {flood} made-up names");
        assert!(refused >= flood - MAX_UNKNOWN, "{refused}");
        p.route("live.apps.test").await.unwrap();
        assert_eq!(asked.load(SeqCst), n, "the live route was pushed out");
    }

    /// A resolver that fails (SERVFAIL) is a failure (502), not a host
    /// without a record (404, remembered for 30 s).
    #[tokio::test]
    async fn a_failing_resolver_is_not_an_answer() {
        let (url, _) = resolver(&[("down.test", "STATUS 2"), ("gone.test", "STATUS 3")]).await;
        let p = proxy(&url, &[], &["i s"]);
        let (status, why) = p.route("down.test").await.unwrap_err();
        assert!(status == StatusCode::BAD_GATEWAY && why.contains("status 2"), "{why}");
        assert_eq!(p.route("gone.test").await.unwrap_err().0, StatusCode::NOT_FOUND);
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
        let proxy = Proxy::new(ProxyConfig { nodes: nodes.clone(), failover_ms: 500, ..Default::default() }).unwrap();
        let object = (0..).map(|i| format!("o{i}")).find(|o| rank(&nodes, "app", o)[0] == nodes[0]).unwrap();
        let req = Request::post(format!("/{object}/charge")).body(Full::new(bytes::Bytes::from("once")).map_err(|e| match e {}).boxed_unsync());
        let t0 = Instant::now();
        let sent = tokio::time::timeout(Duration::from_secs(5), proxy.send("app", &object, req.unwrap())).await;
        let (status, why) = sent.expect("still waiting long past failover_ms").unwrap_err();
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{why}");
        assert!(t0.elapsed() < Duration::from_millis(1500), "{:?}", t0.elapsed());
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
        let r = parse("app=ghcr.io/org/app:prod; trust=https://i s1; alias=WWW.a.com; trust=https://i s2; min=1.2.0; alias=b.com").unwrap();
        assert_eq!(r.app.whole(), "ghcr.io/org/app:prod");
        assert_eq!((r.trust.as_str(), r.min.as_deref()), ("https://i s1, https://i s2", Some("1.2.0")));
        assert_eq!(r.aliases, ["www.a.com", "b.com"]);
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
