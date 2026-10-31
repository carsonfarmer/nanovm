//! WebSockets, held by the proxy and delivered to objects in Pushpin's
//! WebSocket-over-HTTP (https://pushpin.org/docs/protocols/websocket-over-http/).
//! A socket opened at `/{object}/…` opens if the object answers `OPEN`;
//! each event after (`TEXT`, `BINARY`, `CLOSE`, `DISCONNECT`) is a `POST` of
//! `application/websocket-events` with the socket's `Connection-Id`, queued
//! with the object's other requests, and answered with events for the socket.
//!
//! An object that accepts GRIP (answering `Sec-WebSocket-Extensions: grip`)
//! sends `m:` messages and `c:` controls, which subscribe the socket to a
//! channel of its own object. It reaches every subscribed socket by posting
//! GRIP's publish format to `http://nanovm/publish/`.
//!
//! Sockets belong to apps, not workers; events resolve the current version.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use futures::future::{BoxFuture, Fuse, FusedFuture};
use futures::{FutureExt, SinkExt, StreamExt};
use http::header::{CONTENT_TYPE, SEC_WEBSOCKET_EXTENSIONS};
use http::{HeaderMap, HeaderName, HeaderValue, Request, Response, Uri};
use http_body_util::{BodyExt, Full, Limited};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig, frame::coding::CloseCode};
use tokio_tungstenite::tungstenite::{self, Message, Utf8Bytes};

use crate::proxy::Body;
use crate::config::ProxyConfig;

/// Where an object publishes to its sockets.
pub const PUBLISH_HOST: &str = "nanovm";
/// Names the object a publish comes from: the worker sets it, replacing any
/// the guest sent.
pub const OBJECT_HEADER: &str = "nanovm-object";
pub const EVENTS: &str = "application/websocket-events";
const CONNECTION_ID: HeaderName = HeaderName::from_static("connection-id");
/// A write the client doesn't take for this long closes the socket, which
/// would otherwise hold its file descriptor and queue for good.
const STALL: Duration = Duration::from_secs(30);
/// A socket is pinged this often, and closed if it heard nothing since the last.
const PING: Duration = Duration::from_secs(30);
/// Channels per socket, items per publish, and the bytes of a socket's
/// headers: each costs the proxy memory or work.
const MAX_CHANNELS: usize = 64;
const MAX_ITEMS: usize = 64;
const MAX_HEADERS: usize = 16 << 10;
/// What an event's framing adds to the message it carries.
const FRAMING: usize = 32;

#[derive(Debug, PartialEq)]
pub enum Event {
    Open,
    Text(Utf8Bytes),
    Binary(Bytes),
    Close(Option<u16>),
    Disconnect,
}

impl Event {
    /// `KIND\r\n`, or `KIND HEXLEN\r\n`, the content and `\r\n`.
    fn encode(&self) -> Bytes {
        let of = |k: &str, c: &[u8]| Bytes::from([format!("{k} {:x}\r\n", c.len()).as_bytes(), c, b"\r\n"].concat());
        match self {
            Event::Open => "OPEN\r\n".into(),
            Event::Text(t) => of("TEXT", t.as_bytes()),
            Event::Binary(b) => of("BINARY", b),
            Event::Close(Some(c)) => of("CLOSE", &c.to_be_bytes()),
            Event::Close(None) => "CLOSE\r\n".into(),
            Event::Disconnect => "DISCONNECT\r\n".into(),
        }
    }
}

/// The events in `body`, if it holds nothing else.
fn parse(body: Bytes) -> Option<Vec<Event>> {
    let mut events = vec![];
    let mut at = 0;
    while at < body.len() {
        let end = at + body[at..].windows(2).position(|w| w == b"\r\n")?;
        let head = std::str::from_utf8(&body[at..end]).ok()?;
        at = end + 2;
        let (kind, content) = match head.split_once(' ') {
            None => (head, None),
            Some((kind, n)) if n.len() <= 8 && n.bytes().all(|b| b.is_ascii_hexdigit()) => {
                let n = usize::from_str_radix(n, 16).ok()?;
                let content = body.get(at..at + n)?;
                (body.get(at + n..at + n + 2)? == b"\r\n").then_some(())?;
                at += n + 2;
                (kind, Some(body.slice_ref(content)))
            }
            Some(_) => return None,
        };
        events.push(match (kind, content) {
            ("OPEN", _) => Event::Open,
            ("DISCONNECT", _) => Event::Disconnect,
            ("TEXT", Some(c)) => Event::Text(c.try_into().ok()?),
            ("BINARY", Some(c)) => Event::Binary(c),
            ("CLOSE", None) => Event::Close(None),
            ("CLOSE", Some(c)) => Event::Close(Some(u16::from_be_bytes(c[..].try_into().ok()?))),
            _ => return None,
        });
    }
    Some(events)
}

/// An object's answer to an event.
pub struct Answer {
    headers: HeaderMap,
    events: Vec<Event>,
}

/// What became of an event sent to the object.
pub enum Reply {
    Answer(Answer),
    /// The answer wasn't events: the status to refuse a socket with, the
    /// object's own if it refused.
    Bad(u16),
}

/// `res` as events, if it is a `200` of them within `max` bytes.
pub async fn answer<B>(res: Response<B>, max: usize) -> Reply
where
    B: http_body::Body<Data = Bytes>,
    B::Error: Into<crate::object::BoxError>,
{
    let status = res.status();
    if status != 200 || res.headers().get(CONTENT_TYPE).is_none_or(|v| v != EVENTS) {
        return Reply::Bad(if status.is_client_error() || status.is_server_error() { status.as_u16() } else { 502 });
    }
    let (parts, body) = res.into_parts();
    let events = Limited::new(body, max).collect().await.ok().and_then(|b| parse(b.to_bytes()));
    events.map_or(Reply::Bad(502), |events| Reply::Answer(Answer { headers: parts.headers, events }))
}

/// Whether `req` asks to open a WebSocket.
pub fn wants<B>(req: &Request<B>) -> bool {
    req.headers().get(http::header::UPGRADE).is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
}

/// Whether `req` claims to be a socket's event, which only the proxy may
/// send.
pub fn is_event<B>(req: &Request<B>) -> bool {
    req.headers().get(CONTENT_TYPE).is_some_and(|v| v == EVENTS)
}

/// One app's sockets, ordered by object so publishing visits only its own.
pub struct Sockets {
    socks: Mutex<BTreeMap<(String, u64), Sock>>,
    cfg: Arc<ProxyConfig>,
    total: Arc<AtomicUsize>,
}

struct Sock {
    channels: HashSet<String>,
    out: Out,
}

/// Where a socket's messages wait to be written. Their bytes are counted, and
/// a socket more than `max` behind is closed: each socket has a budget of its
/// own, so no client can crowd out another.
#[derive(Clone)]
struct Out {
    tx: mpsc::UnboundedSender<Message>,
    queued: Arc<AtomicUsize>,
    max: usize,
    behind: Arc<Notify>,
}

/// What a queued message costs, an empty one included.
fn cost(m: &Message) -> usize {
    m.len() + 64
}

impl Out {
    fn send(&self, m: Message) {
        let n = cost(&m);
        if self.queued.fetch_add(n, Relaxed) + n <= self.max {
            let _ = self.tx.send(m);
        } else {
            self.behind.notify_one();
        }
    }
}

fn frame(code: CloseCode) -> Option<CloseFrame> {
    Some(CloseFrame { code, reason: "".into() })
}

/// Sends an event's request to the object.
pub type Deliver = Box<dyn Fn(Request<Body>) -> BoxFuture<'static, Reply> + Send + Sync>;

/// A socket, as its events are sent to its object.
pub struct Conn {
    key: (String, u64),
    uri: Uri,
    headers: HeaderMap,
    grip: bool,
    sockets: Arc<Sockets>,
    out: Out,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.sockets.socks.lock().remove(&self.key);
        self.sockets.total.fetch_sub(1, Relaxed);
    }
}

impl Conn {
    fn request(&self, e: &Event) -> Request<Body> {
        let mut req = Request::post(self.uri.clone())
            .body(Full::new(e.encode()).map_err(|e| match e {}).boxed_unsync())
            .expect("a valid request");
        *req.headers_mut() = self.headers.clone();
        req.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static(EVENTS));
        req.headers_mut().insert(CONNECTION_ID, self.key.1.into());
        req
    }

    /// Does what an answer's events ask; `false` if one isn't an event an
    /// answer may carry, which ends the socket.
    fn apply(&self, events: Vec<Event>) -> bool {
        for e in events {
            match e {
                Event::Text(t) if !self.grip => self.out.send(Message::Text(t)),
                Event::Text(t) => match t.as_str().split_at_checked(2) {
                    Some(("m:", m)) => self.out.send(Message::text(m)),
                    Some(("c:", c)) if self.control(c) => {}
                    _ => return false,
                },
                Event::Binary(b) => self.out.send(Message::Binary(b)),
                Event::Close(code) => {
                    let code = code.map(CloseCode::from).filter(|c| c.is_allowed()).unwrap_or(CloseCode::Normal);
                    self.out.send(Message::Close(frame(code)))
                }
                Event::Open | Event::Disconnect => return false,
            }
        }
        true
    }

    /// `{"type": "subscribe" | "unsubscribe", "channel": name}`.
    fn control(&self, c: &str) -> bool {
        let Ok(Value::Object(c)) = serde_json::from_str(c) else { return false };
        let channel = c.get("channel").and_then(Value::as_str).filter(|c| crate::table::valid_name(c));
        let (Some(channel), 2) = (channel, c.len()) else { return false };
        let mut socks = self.sockets.socks.lock();
        let Some(sock) = socks.get_mut(&self.key) else { return false };
        match c.get("type").and_then(Value::as_str) {
            Some("subscribe") if sock.channels.contains(channel) || sock.channels.len() < MAX_CHANNELS => {
                sock.channels.insert(channel.to_string())
            }
            Some("unsubscribe") => sock.channels.remove(channel),
            _ => return false,
        };
        true
    }
}

impl Sockets {
    pub fn new(cfg: Arc<ProxyConfig>, total: Arc<AtomicUsize>) -> Sockets {
        Sockets { socks: Mutex::default(), cfg, total }
    }

    /// Opens a socket on `object` if the caps allow it and the object accepts:
    /// the 101 to answer the upgrade with. `req` has had the app headers
    /// removed; its own WebSocket headers are the client's.
    pub async fn open(
        self: &Arc<Self>,
        object: &str,
        mut req: Request<Body>,
        deliver: Deliver,
    ) -> Result<Response<()>, (u16, &'static str)> {
        let cfg = &self.cfg;
        static NEXT: AtomicU64 = AtomicU64::new(1);
        use http::header::{SEC_WEBSOCKET_KEY as KEY, SEC_WEBSOCKET_PROTOCOL as PROTOCOL};
        let upgrade = req.extensions_mut().remove::<hyper::upgrade::OnUpgrade>();
        let h = req.headers();
        let key = h.get(KEY).filter(|k| B64.decode(k.as_bytes()).is_ok_and(|k| k.len() == 16));
        let (Some(upgrade), Some(key), &http::Method::GET) = (upgrade, key, req.method()) else {
            return Err((400, "not a WebSocket handshake"));
        };
        if h.get(http::header::SEC_WEBSOCKET_VERSION).is_none_or(|v| v != "13") {
            return Err((426, "only WebSocket version 13"));
        }
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        let ours = ["connection", "upgrade", "content-length", "transfer-encoding", "content-type", "connection-id"];
        let hop = ["te", "trailer", "keep-alive", "proxy-connection", "expect", "content-encoding"];
        let handshake = |k: &HeaderName| k.as_str().starts_with("sec-websocket-") && k != PROTOCOL;
        let kept = h.iter().filter(|(k, _)| !ours.contains(&k.as_str()) && !hop.contains(&k.as_str()) && !handshake(k));
        let headers: HeaderMap = kept.map(|(k, v)| (k.clone(), v.clone())).collect();
        // Kept for the socket's life, so bounded.
        if headers.iter().map(|(k, v)| k.as_str().len() + v.len()).sum::<usize>() > MAX_HEADERS {
            return Err((431, "the handshake's headers are too large"));
        }

        if self.total.fetch_add(1, Relaxed) >= cfg.max_sockets {
            self.total.fetch_sub(1, Relaxed);
            return Err((503, "the proxy has max_sockets open"));
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let out = Out {
            tx,
            queued: Default::default(),
            max: (cfg.socket_backlog_kb << 10) as usize,
            behind: Default::default(),
        };
        let id = NEXT.fetch_add(1, Relaxed);
        let mut conn = Conn {
            key: (object.to_string(), id),
            uri: req.uri().clone(),
            headers,
            grip: false,
            sockets: self.clone(),
            out: out.clone(),
        };
        {
            let mut socks = self.socks.lock();
            if socks.len() >= cfg.max_sockets_per_app {
                return Err((503, "the app has max_sockets open"));
            }
            if socks.range((object.to_string(), 0)..=(object.to_string(), u64::MAX)).count() >= cfg.max_sockets_per_object {
                return Err((503, "the object has max_sockets open"));
            }
            socks.insert(conn.key.clone(), Sock { channels: HashSet::new(), out });
        }

        let mut open = conn.request(&Event::Open);
        open.headers_mut().insert(SEC_WEBSOCKET_EXTENSIONS, HeaderValue::from_static("grip"));
        let (headers, events) = match deliver(open).await {
            Reply::Answer(Answer { headers, events }) => (headers, events),
            Reply::Bad(status) => return Err((status, "the object refused the socket")),
        };
        conn.grip = headers.get(SEC_WEBSOCKET_EXTENSIONS).is_some_and(|e| e == "grip");
        let protocol = headers.get(PROTOCOL);
        let offered = |p: &HeaderValue| {
            let offers = req.headers().get_all(PROTOCOL).iter().filter_map(|v| v.to_str().ok());
            offers.flat_map(|v| v.split(',')).any(|o| o.trim().as_bytes() == p.as_bytes())
        };
        let mut events = events.into_iter();
        if events.next() != Some(Event::Open) {
            return Err((502, "the object did not accept the socket"));
        }
        // It accepted, so it is told the socket ended.
        if !protocol.is_none_or(offered) || !conn.apply(events.collect()) {
            deliver(conn.request(&Event::Disconnect)).await;
            return Err((502, "the object's answer to OPEN is not one a socket can take"));
        }

        let mut res = Response::builder()
            .status(101)
            .header(http::header::CONNECTION, "upgrade")
            .header(http::header::UPGRADE, "websocket")
            .header(http::header::SEC_WEBSOCKET_ACCEPT, accept);
        if let Some(p) = protocol {
            res = res.header(PROTOCOL, p);
        }
        let max = ((cfg.max_request_body_kb << 10) as usize).saturating_sub(FRAMING);
        let config = WebSocketConfig::default().max_message_size(Some(max)).max_frame_size(Some(max));
        tokio::spawn(async move {
            match upgrade.await {
                Ok(io) => run(conn, hyper_util::rt::TokioIo::new(io), rx, config, deliver).await,
                Err(_) => drop(deliver(conn.request(&Event::Disconnect)).await),
            }
        });
        Ok(res.body(()).expect("a valid response"))
    }

    /// Sends what `object` publishes to its sockets subscribed to each item's
    /// channel: GRIP's `{"items": [{"channel", "formats": {"ws-message":
    /// {"content" | "content-bin"}}}]}`. The status to answer with.
    pub async fn publish<B>(&self, req: Request<B>) -> u16
    where
        B: http_body::Body<Data = Bytes>,
        B::Error: Into<crate::object::BoxError>,
    {
        let object = req.headers().get(OBJECT_HEADER).and_then(|v| v.to_str().ok()).map(str::to_string);
        let body = tokio::time::timeout(STALL, Limited::new(req.into_body(), (self.cfg.max_response_body_kb << 10) as usize).collect()).await;
        let body = match body { Ok(Ok(body)) => body, Ok(Err(_)) => return 413, Err(_) => return 408 };
        let (Some(object), Some(items)) = (object, messages(&body.to_bytes())) else { return 400 };
        for (_, sock) in self.socks.lock().range((object.clone(), 0)..=(object, u64::MAX)) {
            for (_, m) in items.iter().filter(|(c, _)| sock.channels.contains(c)) {
                sock.out.send(m.clone());
            }
        }
        200
    }
}

/// Whether a publish's body is one [`Sockets::publish`] takes.
pub fn well_formed(json: &[u8]) -> bool {
    messages(json).is_some()
}

/// The (channel, message) of each item of a publish that has one for a
/// socket; `None` unless all are well formed.
fn messages(json: &[u8]) -> Option<Vec<(String, Message)>> {
    let publish: Value = serde_json::from_slice(json).ok()?;
    let items = publish.get("items")?.as_array().filter(|i| i.len() <= MAX_ITEMS)?;
    let message = |item: &Value| -> Option<Option<(String, Message)>> {
        let channel = item.get("channel")?.as_str()?.to_string();
        let Some(m) = item.get("formats")?.get("ws-message") else { return Some(None) };
        let m = m.as_object().filter(|m| m.len() == 1)?;
        let m = match (m.get("content"), m.get("content-bin")) {
            (Some(t), None) => Message::text(t.as_str()?),
            (None, Some(b)) => Message::binary(B64.decode(b.as_str()?).ok()?),
            _ => return None,
        };
        Some(Some((channel, m)))
    };
    items.iter().map(message).collect::<Option<Vec<_>>>().map(|m| m.into_iter().flatten().collect())
}

/// Serves a socket until it ends. The client's messages go to the object one
/// at a time: the next is read once the last is answered. What is queued for
/// the client is written meanwhile. The object is told how the socket ended,
/// unless it closed it or its worker is gone.
async fn run<Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    conn: Conn,
    io: Io,
    mut rx: mpsc::UnboundedReceiver<Message>,
    config: WebSocketConfig,
    deliver: Deliver,
) {
    let (mut sink, mut stream) = WebSocketStream::from_raw_socket(io, Role::Server, Some(config)).await.split();
    let mut pending = Fuse::terminated();
    let (mut ping, mut heard) = (tokio::time::interval(PING), true);
    let (close, tell) = loop {
        tokio::select! {
            m = stream.next(), if pending.is_terminated() => {
                heard = true;
                let event = match m {
                    Some(Ok(Message::Text(t))) => Event::Text(t),
                    Some(Ok(Message::Binary(b))) => Event::Binary(b),
                    Some(Ok(Message::Close(f))) => {
                        // Sends the reply the socket queued.
                        let _ = tokio::time::timeout(STALL, sink.flush()).await;
                        break (None, Some(Event::Close(f.map(|f| f.code.into()))));
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(tungstenite::Error::Capacity(_))) => break (frame(CloseCode::Size), Some(Event::Disconnect)),
                    _ => break (None, Some(Event::Disconnect)),
                };
                pending = deliver(conn.request(&event)).fuse();
            }
            reply = &mut pending => match reply {
                Reply::Answer(a) => {
                    if !conn.apply(a.events) {
                        break (frame(CloseCode::Error), Some(Event::Disconnect));
                    }
                }
                _ => break (frame(CloseCode::Error), Some(Event::Disconnect)),
            },
            Some(m) = rx.recv() => {
                let (n, close) = (cost(&m), matches!(m, Message::Close(_)));
                let sent = tokio::select! {
                    sent = tokio::time::timeout(STALL, sink.send(m)) => matches!(sent, Ok(Ok(()))),
                    _ = conn.out.behind.notified() => false,
                };
                // A client this far behind can't take a close frame either.
                if !sent {
                    break (None, Some(Event::Disconnect));
                }
                conn.out.queued.fetch_sub(n, Relaxed);
                if close {
                    break (None, None);
                }
            }
            _ = conn.out.behind.notified() => break (None, Some(Event::Disconnect)),
            // A client silent through a whole interval (while nothing waits
            // on the object) is gone, even if its connection never said so.
            _ = ping.tick() => {
                // The stream isn't read while an event waits, so a pong can't
                // be heard then.
                if pending.is_terminated() {
                    if !heard {
                        break (None, Some(Event::Disconnect));
                    }
                    heard = false;
                }
                conn.out.send(Message::Ping(Bytes::new()));
            }
        }
    };
    if let Some(f) = close {
        let _ = tokio::time::timeout(STALL, sink.send(Message::Close(Some(f)))).await;
    }
    drop((sink, stream, pending));
    if let Some(e) = tell {
        deliver(conn.request(&e)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip_and_parse_strictly() {
        let all = vec![
            Event::Open,
            Event::Text("héllo\r\n".into()),
            Event::Binary(Bytes::from_static(&[0, 13, 10])),
            Event::Close(Some(1001)),
            Event::Close(None),
            Event::Disconnect,
        ];
        let body: Vec<u8> = all.iter().flat_map(|e| e.encode()).collect();
        assert_eq!(parse(body.into()), Some(all));
        let bad: [&[u8]; 7] = [
            b"TEXT\r\n",
            b"TEXT 3\r\nab\r\n",
            b"TEXT 2\r\nab",
            b"PING\r\n",
            b"OPEN",
            b"TEXT 1\r\n\xff\r\n",
            b"CLOSE 1\r\na\r\n",
        ];
        for b in bad {
            assert_eq!(parse(Bytes::copy_from_slice(b)), None, "{b:?}");
        }
    }

    /// Every message counts against the socket's backlog, an empty one too.
    #[test]
    fn backlogs_count_every_message() {
        let (tx, rx) = mpsc::unbounded_channel();
        let out = Out { tx, queued: Default::default(), max: 1000, behind: Default::default() };
        for _ in 0..15 {
            out.send(Message::text(""));
        }
        assert_eq!(rx.len(), 15);
        out.send(Message::text(""));
        assert_eq!(rx.len(), 15, "a socket 1000 bytes behind takes no more");
        assert!(out.behind.notified().now_or_never().is_some());
    }

    /// A client that goes silent, as a half-open connection does, is closed
    /// after a ping goes unanswered, and its object is told.
    #[tokio::test(start_paused = true)]
    async fn silent_clients_are_dropped() {
        let total = Arc::new(AtomicUsize::new(1));
        let sockets = Arc::new(Sockets::new(caps(1, 1, 1), total.clone()));
        let (tx, rx) = mpsc::unbounded_channel();
        let out = Out { tx, queued: Default::default(), max: 1 << 20, behind: Default::default() };
        let conn = Conn {
            key: ("o".into(), 1),
            uri: "/a/o/".parse().unwrap(),
            headers: HeaderMap::new(),
            grip: false,
            sockets,
            out,
        };
        let told = Arc::new(Mutex::new(vec![]));
        let t = told.clone();
        let deliver: Deliver = Box::new(move |req| {
            t.lock().push(req.into_body());
            Box::pin(async { Reply::Bad(502) })
        });
        let (_client, server) = tokio::io::duplex(1 << 16);
        let ran = tokio::spawn(run(conn, server, rx, WebSocketConfig::default(), deliver));
        tokio::time::sleep(PING * 2 + Duration::from_secs(1)).await;
        assert!(ran.is_finished(), "a silent client is still open");
        assert_eq!(told.lock().len(), 1, "the object is told");
        assert_eq!(total.load(Relaxed), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn slow_publishes_have_a_deadline() {
        let sockets = Sockets::new(caps(10, 10, 10), Arc::default());
        let body = http_body_util::StreamBody::new(futures::stream::pending::<Result<http_body::Frame<Bytes>, std::io::Error>>());
        let req = Request::post("/publish").header(OBJECT_HEADER, "o").body(body).unwrap();
        assert_eq!(tokio::time::timeout(STALL * 2, sockets.publish(req)).await.unwrap(), 408);
    }

    #[test]
    fn publishes_take_only_well_formed_items() {
        let ok = br#"{"items":[{"channel":"a","formats":{"ws-message":{"content":"hi"}}},
            {"channel":"b","formats":{"ws-message":{"content-bin":"AAE="}}},{"channel":"c","formats":{"http-stream":{}}}]}"#;
        let got = messages(ok).unwrap();
        assert_eq!(got, vec![("a".into(), Message::text("hi")), ("b".into(), Message::binary(vec![0, 1]))]);
        for bad in
            [r#"{}"#, r#"{"items":[{"formats":{}}]}"#, r#"{"items":[{"channel":"a","formats":{"ws-message":{}}}]}"#]
        {
            assert!(messages(bad.as_bytes()).is_none(), "{bad}");
        }
    }

    fn caps(max_total: usize, per_app: usize, per_object: usize) -> Arc<ProxyConfig> {
        Arc::new(ProxyConfig {
            max_sockets: max_total,
            max_sockets_per_app: per_app,
            max_sockets_per_object: per_object,
            max_request_body_kb: 64,
            max_response_body_kb: 1,
            ..Default::default()
        })
    }

    fn out() -> (Out, mpsc::UnboundedReceiver<Message>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Out {
                tx,
                queued: Default::default(),
                max: 1 << 20,
                behind: Default::default(),
            },
            rx,
        )
    }

    fn sock(channels: &[&str], out: Out) -> Sock {
        Sock {
            channels: channels.iter().map(|c| c.to_string()).collect(),
            out,
        }
    }

    /// An open socket on object `o`, and its client's queue.
    fn conn(grip: bool) -> (Conn, mpsc::UnboundedReceiver<Message>) {
        let sockets = Arc::new(Sockets::new(caps(1, 1, 1), Arc::default()));
        sockets.total.store(1, Relaxed);
        let (out, rx) = out();
        sockets.socks.lock().insert(("o".into(), 1), sock(&[], out.clone()));
        let uri = "/a/o/".parse().unwrap();
        (
            Conn {
                key: ("o".into(), 1),
                uri,
                headers: HeaderMap::new(),
                grip,
                sockets,
                out,
            },
            rx,
        )
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<Message>) -> Vec<Message> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn channels(c: &Conn) -> Vec<String> {
        let mut v: Vec<_> = c.sockets.socks.lock()[&c.key].channels.iter().cloned().collect();
        v.sort();
        v
    }

    /// Without GRIP, text and binary reach the client as they are, prefix
    /// and all; an answer may not carry OPEN or DISCONNECT.
    #[test]
    fn apply_without_grip_passes_messages_through() {
        let (c, mut rx) = conn(false);
        let events = vec![
            Event::Text("m:hi".into()),
            Event::Text("c:{}".into()),
            Event::Binary(Bytes::from_static(b"\0")),
        ];
        assert!(c.apply(events));
        assert_eq!(
            drain(&mut rx),
            [Message::text("m:hi"), Message::text("c:{}"), Message::binary(vec![0])]
        );
        assert!(channels(&c).is_empty());
        assert!(!c.apply(vec![Event::Open]));
        assert!(!c.apply(vec![Event::Disconnect]));
    }

    /// With GRIP, text is an `m:` message (sent without its prefix) or a
    /// `c:` control (sent nowhere); any other text ends the socket. Binary
    /// passes as it is.
    #[test]
    fn apply_with_grip_takes_messages_and_controls() {
        let (c, mut rx) = conn(true);
        let sub = r#"c:{"type":"subscribe","channel":"news"}"#;
        let events = vec![
            Event::Text("m:hello".into()),
            Event::Text("m:".into()),
            Event::Text(sub.into()),
            Event::Binary(Bytes::from_static(b"b")),
        ];
        assert!(c.apply(events));
        assert_eq!(
            drain(&mut rx),
            [Message::text("hello"), Message::text(""), Message::binary(&b"b"[..])]
        );
        assert_eq!(channels(&c), ["news"]);
        for bad in ["hello", "m", "", "é", "€", "x:y", "M:hi", "c:"] {
            assert!(!c.apply(vec![Event::Text(bad.into())]), "{bad:?}");
        }
        assert!(drain(&mut rx).is_empty());
    }

    /// A control is exactly `{"type", "channel"}`, with a valid channel name
    /// and a known type; unsubscribing from a channel never joined is fine.
    #[test]
    fn controls_are_strict() {
        let (c, _rx) = conn(true);
        assert!(c.control(r#"{"type":"subscribe","channel":"a"}"#));
        assert!(c.control(r#"{"channel":"a","type":"subscribe"}"#), "again is fine");
        assert_eq!(channels(&c), ["a"]);
        assert!(c.control(r#"{"type":"unsubscribe","channel":"a"}"#));
        assert!(c.control(r#"{"type":"unsubscribe","channel":"never"}"#));
        for bad in [
            "",
            "null",
            "[]",
            "{",
            r#""subscribe""#,
            r#"{"type":"subscribe"}"#,
            r#"{"channel":"a"}"#,
            r#"{"type":"subscribe","channel":"a","x":1}"#,
            r#"{"type":"subscribe","channel":"a/b"}"#,
            r#"{"type":"subscribe","channel":".a"}"#,
            r#"{"type":"subscribe","channel":""}"#,
            r#"{"type":"subscribe","channel":1}"#,
            r#"{"type":"join","channel":"a"}"#,
            r#"{"type":"Subscribe","channel":"a"}"#,
            r#"{"kind":"subscribe","channel":"a"}"#,
        ] {
            assert!(!c.control(bad), "{bad}");
        }
        assert!(channels(&c).is_empty());
        c.sockets.socks.lock().remove(&c.key);
        assert!(
            !c.control(r#"{"type":"subscribe","channel":"a"}"#),
            "the socket is gone"
        );
    }

    /// A socket joins at most `MAX_CHANNELS` channels; at the cap it may
    /// still repeat one it has, and leaving one makes room.
    #[test]
    fn channels_per_socket_are_capped() {
        let (c, _rx) = conn(true);
        let ctl = |t: &str, ch: &str| c.control(&serde_json::json!({"type": t, "channel": ch}).to_string());
        for i in 0..MAX_CHANNELS {
            assert!(ctl("subscribe", &format!("c{i}")));
        }
        assert!(!ctl("subscribe", "one-more"));
        assert!(ctl("subscribe", "c0"), "an existing channel at the cap");
        assert!(ctl("unsubscribe", "c0"));
        assert!(ctl("subscribe", "one-more"));
        assert_eq!(channels(&c).len(), MAX_CHANNELS);
    }

    /// The object's close code is sent if a peer may send it; codes only an
    /// endpoint may report (1005, 1006, 1015), reserved and out-of-range
    /// ones, and none at all, become a normal close.
    #[test]
    fn close_codes_are_sanitised() {
        let (c, mut rx) = conn(false);
        for code in [
            1000, 1001, 1002, 1003, 1007, 1008, 1009, 1010, 1011, 1012, 1013, 3000, 3999, 4000, 4999,
        ] {
            assert!(c.apply(vec![Event::Close(Some(code))]));
            assert_eq!(drain(&mut rx), [Message::Close(frame(code.into()))], "{code}");
        }
        for code in [
            None,
            Some(0),
            Some(999),
            Some(1004),
            Some(1005),
            Some(1006),
            Some(1014),
            Some(1015),
            Some(2999),
            Some(5000),
        ] {
            assert!(c.apply(vec![Event::Close(code)]));
            assert_eq!(drain(&mut rx), [Message::Close(frame(CloseCode::Normal))], "{code:?}");
        }
    }

    /// A publish is taken whole or not at all: an empty list is fine, at
    /// most `MAX_ITEMS` items, and one bad item spoils the rest.
    #[test]
    fn publishes_are_all_or_nothing() {
        use serde_json::json;
        let item = |m: Value| json!({"channel": "a", "formats": {"ws-message": m}});
        let publish = |items: &[Value]| json!({ "items": items }).to_string();
        let good = item(json!({"content": "x"}));
        assert_eq!(messages(publish(&[]).as_bytes()), Some(vec![]));
        let mut full = vec![good.clone(); MAX_ITEMS];
        assert_eq!(messages(publish(&full).as_bytes()).unwrap().len(), MAX_ITEMS);
        full.push(good.clone());
        assert!(messages(publish(&full).as_bytes()).is_none(), "too many items");
        for bad in [
            item(json!({"content": "x", "content-bin": "AA=="})),
            item(json!({"content": "x", "extra": 1})),
            item(json!({"content": 1})),
            item(json!({"content-bin": "not base64!"})),
            item(json!({"content-bin": [0]})),
            item(json!("x")),
            json!({"channel": 1, "formats": {"ws-message": {"content": "x"}}}),
            json!({"formats": {"ws-message": {"content": "x"}}}),
            json!({"channel": "a"}),
            json!("item"),
        ] {
            assert!(
                messages(publish(&[good.clone(), bad.clone()]).as_bytes()).is_none(),
                "{bad}"
            );
        }
        for bad in ["", "not json", "[]", r#"{"items":{}}"#, r#"{"items":null}"#] {
            assert!(messages(bad.as_bytes()).is_none(), "{bad}");
        }
    }

    /// A publish reaches its own object's sockets on each item's channel,
    /// and only those; one without the object's name or well-formed items
    /// is a 400, and one over `response_max` a 413, reaching no one.
    #[tokio::test]
    async fn publishes_reach_only_the_objects_subscribers() {
        let sockets = Sockets::new(caps(10, 10, 10), Arc::default());
        let mut rxs = vec![];
        for (id, object, chans) in [
            (1, "o", &["a"][..]),
            (2, "o", &["b"]),
            (3, "p", &["a", "b"]),
            (4, "o", &["a", "b"]),
            (5, "o", &[]),
        ] {
            let (out, rx) = out();
            sockets.socks.lock().insert((object.into(), id), sock(chans, out));
            rxs.push(rx);
        }
        let req = |object: Option<&str>, body: String| {
            let mut r = Request::post("http://nanovm/publish/")
                .body(Full::new(Bytes::from(body)))
                .unwrap();
            if let Some(o) = object {
                r.headers_mut().insert(OBJECT_HEADER, o.parse().unwrap());
            }
            r
        };
        let body = r#"{"items":[{"channel":"a","formats":{"ws-message":{"content":"A"}}},
            {"channel":"b","formats":{"ws-message":{"content":"B"}}}]}"#;
        assert_eq!(sockets.publish(req(Some("o"), body.into())).await, 200);
        let got: Vec<_> = rxs.iter_mut().map(drain).collect();
        let (a, b) = (Message::text("A"), Message::text("B"));
        assert_eq!(got, [vec![a.clone()], vec![b.clone()], vec![], vec![a, b], vec![]]);

        assert_eq!(sockets.publish(req(None, body.into())).await, 400);
        assert_eq!(sockets.publish(req(Some("o"), "{}".into())).await, 400);
        assert_eq!(sockets.publish(req(Some("o"), format!("{body:1100}"))).await, 413);
        assert!(rxs.iter_mut().all(|rx| drain(rx).is_empty()));
    }

    /// An upgrade is any case of `websocket`; an event is exactly the events
    /// content type.
    #[test]
    fn upgrades_and_events_are_recognised() {
        let req = |k: HeaderName, v: &'static str| Request::get("/").header(k, v).body(()).unwrap();
        for v in ["websocket", "WebSocket"] {
            assert!(wants(&req(http::header::UPGRADE, v)));
        }
        assert!(!wants(&req(http::header::UPGRADE, "h2c")) && !wants(&Request::new(())));
        assert!(is_event(&req(CONTENT_TYPE, EVENTS)));
        assert!(!is_event(&req(CONTENT_TYPE, "application/websocket-events; x=1")) && !is_event(&Request::new(())));
    }

    type Log = Arc<Mutex<Vec<(HeaderMap, Bytes)>>>;

    /// Answers every event with `reply()`, recording what was sent.
    fn deliver(reply: impl Fn() -> Reply + Send + Sync + 'static) -> (Deliver, Log) {
        let log = Log::default();
        let l = log.clone();
        let d: Deliver = Box::new(move |req| {
            let (l, reply) = (l.clone(), reply());
            Box::pin(async move {
                let (parts, body) = req.into_parts();
                let body = body.collect().await.unwrap().to_bytes();
                l.lock().push((parts.headers, body));
                reply
            })
        });
        (d, log)
    }

    fn accept(
        headers: &[(&'static str, &'static str)],
        events: impl Fn() -> Vec<Event> + Send + Sync + 'static,
    ) -> impl Fn() -> Reply + Send + Sync + 'static {
        let headers: HeaderMap = headers
            .iter()
            .map(|(k, v)| (HeaderName::from_static(k), HeaderValue::from_static(v)))
            .collect();
        move || {
            Reply::Answer(Answer {
                headers: headers.clone(),
                events: events(),
            })
        }
    }

    /// The RFC 6455 example handshake, with headers of the client's own and
    /// ones that are not the object's to see.
    fn handshake() -> Request<Body> {
        let mut req = Request::get("/a/o/chat")
            .header("upgrade", "websocket")
            .header("connection", "Upgrade")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-protocol", "chat, superchat")
            .header("sec-websocket-extensions", "permessage-deflate")
            .header("connection-id", "spoofed")
            .header("te", "trailers")
            .header("x-user", "u1")
            .body(http_body_util::Empty::new().map_err(|e| match e {}).boxed_unsync())
            .unwrap();
        req.extensions_mut().insert(hyper::upgrade::on(Request::new(())));
        req
    }

    async fn try_open(
        s: &Arc<Sockets>,
        req: Request<Body>,
        reply: impl Fn() -> Reply + Send + Sync + 'static,
    ) -> (Result<Response<()>, (u16, &'static str)>, Log) {
        let (d, log) = deliver(reply);
        (s.open("o", req, d).await, log)
    }

    /// Anything but a version-13 WebSocket handshake of bounded size is
    /// refused before the object is asked, holding no socket.
    #[tokio::test]
    async fn bad_handshakes_are_refused() {
        let s = Arc::new(Sockets::new(caps(10, 10, 10), Arc::default()));
        type Edit = fn(&mut Request<Body>);
        let edit: [(Edit, u16); 8] = [
            (|r| r.extensions_mut().clear(), 400),
            (|r| *r.method_mut() = http::Method::POST, 400),
            (|r| _ = r.headers_mut().remove("sec-websocket-key"), 400),
            (
                |r| {
                    _ = r
                        .headers_mut()
                        .insert("sec-websocket-key", HeaderValue::from_static("c2hvcnQ="))
                },
                400,
            ),
            (
                |r| {
                    _ = r
                        .headers_mut()
                        .insert("sec-websocket-key", HeaderValue::from_static("!!not base64!!"))
                },
                400,
            ),
            (|r| _ = r.headers_mut().remove("sec-websocket-version"), 426),
            (
                |r| {
                    _ = r
                        .headers_mut()
                        .insert("sec-websocket-version", HeaderValue::from_static("8"))
                },
                426,
            ),
            (
                |r| {
                    _ = r
                        .headers_mut()
                        .insert("x-big", "x".repeat(MAX_HEADERS).parse().unwrap())
                },
                431,
            ),
        ];
        for (i, (edit, status)) in edit.into_iter().enumerate() {
            let mut req = handshake();
            edit(&mut req);
            let (res, log) = try_open(&s, req, accept(&[], || vec![Event::Open])).await;
            assert_eq!(res.unwrap_err().0, status, "case {i}");
            assert!(log.lock().is_empty(), "case {i}");
        }
        assert_eq!(s.total.load(Relaxed), 0);
        assert!(s.socks.lock().is_empty());
    }

    /// The node's, the app's and the object's socket caps each refuse a
    /// socket with a 503, and leave the count as it was.
    #[tokio::test]
    async fn socket_caps_are_enforced() {
        let open = || accept(&[], || vec![Event::Open]);
        let node = Arc::new(Sockets::new(caps(1, 10, 10), Arc::default()));
        node.total.store(1, Relaxed);
        assert_eq!(
            try_open(&node, handshake(), open()).await.0.unwrap_err(),
            (503, "the proxy has max_sockets open")
        );
        assert_eq!(node.total.load(Relaxed), 1);

        let app = Arc::new(Sockets::new(caps(10, 1, 10), Arc::default()));
        app.socks.lock().insert(("other".into(), 0), sock(&[], out().0));
        assert_eq!(
            try_open(&app, handshake(), open()).await.0.unwrap_err(),
            (503, "the app has max_sockets open")
        );
        assert_eq!(app.total.load(Relaxed), 0);

        let object = Arc::new(Sockets::new(caps(10, 10, 1), Arc::default()));
        object.socks.lock().insert(("o".into(), 0), sock(&[], out().0));
        let (res, log) = try_open(&object, handshake(), open()).await;
        assert_eq!(res.unwrap_err(), (503, "the object has max_sockets open"));
        assert!(log.lock().is_empty());
        assert_eq!((object.total.load(Relaxed), object.socks.lock().len()), (0, 1));
    }

    /// An object refuses a socket with its own error status, by answering
    /// anything but OPEN first, by choosing a subprotocol the client didn't
    /// offer, or with events a socket can't take; once it has said OPEN it
    /// is told the socket is gone.
    #[tokio::test]
    async fn objects_refuse_sockets() {
        let s = Arc::new(Sockets::new(caps(10, 10, 10), Arc::default()));
        let refuse = async |reply: Box<dyn Fn() -> Reply + Send + Sync>| {
            let (res, log) = try_open(&s, handshake(), reply).await;
            let told: Vec<_> = log.lock().iter().map(|(_, b)| b.clone()).collect();
            assert_eq!((s.total.load(Relaxed), s.socks.lock().len()), (0, 0));
            (res.unwrap_err(), told)
        };
        let (open, gone) = (Bytes::from("OPEN\r\n"), Bytes::from("DISCONNECT\r\n"));
        let not_events = (403, "the object refused the socket");
        assert_eq!(
            refuse(Box::new(|| Reply::Bad(403))).await,
            (not_events, vec![open.clone()])
        );
        let not_open = (502, "the object did not accept the socket");
        assert_eq!(
            refuse(Box::new(accept(&[], Vec::new))).await,
            (not_open, vec![open.clone()])
        );
        assert_eq!(
            refuse(Box::new(accept(&[], || vec![Event::Text("hi".into())]))).await.0,
            not_open
        );
        let bad = (502, "the object's answer to OPEN is not one a socket can take");
        let both = vec![open.clone(), gone.clone()];
        let protocol = accept(&[("sec-websocket-protocol", "chatty")], || vec![Event::Open]);
        assert_eq!(refuse(Box::new(protocol)).await, (bad, both.clone()));
        assert_eq!(
            refuse(Box::new(accept(&[], || vec![Event::Open, Event::Open]))).await,
            (bad, both.clone())
        );
        let grip = accept(&[("sec-websocket-extensions", "grip")], || {
            vec![Event::Open, Event::Text("hi".into())]
        });
        assert_eq!(refuse(Box::new(grip)).await, (bad, both));
    }

    /// An accepted socket answers the upgrade with the right accept key and
    /// the object's subprotocol; the object sees the client's own headers,
    /// never the handshake's or a spoofed connection id, and is offered
    /// GRIP. A failed upgrade tells it the socket is gone and frees it.
    #[tokio::test]
    async fn accepted_sockets_upgrade() {
        let s = Arc::new(Sockets::new(caps(10, 10, 10), Arc::default()));
        let headers = [
            ("sec-websocket-extensions", "grip"),
            ("sec-websocket-protocol", "superchat"),
        ];
        let (res, log) = try_open(
            &s,
            handshake(),
            accept(&headers, || vec![Event::Open, Event::Text("m:hi".into())]),
        )
        .await;
        let res = res.unwrap();
        assert_eq!(res.status(), 101);
        let h = res.headers();
        assert_eq!(h["upgrade"], "websocket");
        assert_eq!(h["connection"], "upgrade");
        assert_eq!(h["sec-websocket-accept"], "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        assert_eq!(h["sec-websocket-protocol"], "superchat");

        let (sent, body) = log.lock()[0].clone();
        assert_eq!(body, "OPEN\r\n");
        assert_eq!(sent[CONTENT_TYPE], EVENTS);
        assert_eq!(sent[SEC_WEBSOCKET_EXTENSIONS], "grip");
        assert_eq!(sent["x-user"], "u1");
        assert_eq!(sent["sec-websocket-protocol"], "chat, superchat");
        assert!(sent["connection-id"].to_str().unwrap().parse::<u64>().is_ok());
        for gone in [
            "upgrade",
            "connection",
            "sec-websocket-key",
            "sec-websocket-version",
            "te",
        ] {
            assert!(!sent.contains_key(gone), "{gone}");
        }

        for _ in 0..100 {
            if s.total.load(Relaxed) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!((s.total.load(Relaxed), s.socks.lock().len()), (0, 0));
        let log = log.lock();
        assert_eq!((log.len(), &log[1].1), (2, &Bytes::from("DISCONNECT\r\n")));
        assert_eq!(log[1].0["connection-id"], sent["connection-id"]);
    }
}
