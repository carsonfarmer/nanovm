//! WebSockets, held by the supervisor and delivered to objects in Pushpin's
//! WebSocket-over-HTTP (https://pushpin.org/docs/protocols/websocket-over-http/).
//! A socket opened at `/{app}/{object}/…` opens if the object answers `OPEN`;
//! each event after (`TEXT`, `BINARY`, `CLOSE`, `DISCONNECT`) is a `POST` of
//! `application/websocket-events` with the socket's `Connection-Id`, queued
//! with the object's other requests, and answered with events for the socket.
//!
//! An object that accepts GRIP (answering `Sec-WebSocket-Extensions: grip`)
//! sends `m:` messages and `c:` controls, which subscribe the socket to a
//! channel of its own object. It reaches every subscribed socket by posting
//! GRIP's publish format to `http://nanovm/publish/`.
//!
//! A socket belongs to the worker process it opened on and closes with it, so
//! a version switch closes the app's sockets and clients reconnect to the new
//! version.

use std::collections::{HashMap, HashSet};
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
use tokio::sync::{Notify, mpsc, watch};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig, frame::coding::CloseCode};
use tokio_tungstenite::tungstenite::{self, Message, Utf8Bytes};

use crate::supervisor::Body;

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
/// headers: each costs the supervisor memory or work.
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
    /// The socket's worker is no longer the app's.
    Gone,
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

/// Whether `req` claims to be a socket's event, which only the supervisor may
/// send.
pub fn is_event<B>(req: &Request<B>) -> bool {
    req.headers().get(CONTENT_TYPE).is_some_and(|v| v == EVENTS)
}

/// One worker's open sockets, and the channels of each object.
pub struct Sockets {
    inner: Mutex<Inner>,
    /// Sockets the app may hold.
    max: usize,
}

#[derive(Default)]
struct Inner {
    socks: HashMap<u64, Sock>,
    /// Sockets per object.
    objects: HashMap<String, usize>,
    /// `(object, channel)` → its subscribers.
    channels: HashMap<(String, String), HashSet<u64>>,
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

/// The limits a socket opens under.
pub struct Caps<'a> {
    pub total: &'a Arc<AtomicUsize>,
    pub max_total: usize,
    pub per_object: usize,
    /// The most a message may carry from the client.
    pub request_max: usize,
    /// The most an answer may carry.
    pub response_max: usize,
    /// The most a socket may fall behind by.
    pub backlog: usize,
}

/// Sends an event's request to the object.
pub type Deliver = Box<dyn Fn(Request<Body>) -> BoxFuture<'static, Reply> + Send + Sync>;

/// A socket, as its events are sent to its object.
pub struct Conn {
    id: u64,
    object: String,
    uri: Uri,
    headers: HeaderMap,
    grip: bool,
    sockets: Arc<Sockets>,
    out: Out,
    total: Arc<AtomicUsize>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        let mut inner = self.sockets.inner.lock();
        if let Some(sock) = inner.socks.remove(&self.id) {
            for c in sock.channels {
                inner.unsubscribe(&self.object, &c, self.id);
            }
            if let Some(n) = inner.objects.get_mut(&self.object) {
                *n -= 1;
                if *n == 0 {
                    inner.objects.remove(&self.object);
                }
            }
        }
        drop(inner);
        self.total.fetch_sub(1, Relaxed);
    }
}

impl Inner {
    fn unsubscribe(&mut self, object: &str, channel: &str, id: u64) {
        let key = (object.to_string(), channel.to_string());
        if let Some(subs) = self.channels.get_mut(&key) {
            subs.remove(&id);
            if subs.is_empty() {
                self.channels.remove(&key);
            }
        }
    }
}

impl Conn {
    fn request(&self, e: &Event) -> Request<Body> {
        let mut req = Request::post(self.uri.clone())
            .body(Full::new(e.encode()).map_err(|e| match e {}).boxed_unsync())
            .expect("a valid request");
        *req.headers_mut() = self.headers.clone();
        req.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static(EVENTS));
        req.headers_mut().insert(CONNECTION_ID, self.id.into());
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
        let mut inner = self.sockets.inner.lock();
        let Some(sock) = inner.socks.get_mut(&self.id) else { return false };
        match c.get("type").and_then(Value::as_str) {
            Some("subscribe") if sock.channels.contains(channel) => {}
            Some("subscribe") if sock.channels.len() < MAX_CHANNELS => {
                sock.channels.insert(channel.to_string());
                let key = (self.object.clone(), channel.to_string());
                inner.channels.entry(key).or_default().insert(self.id);
            }
            Some("unsubscribe") => {
                sock.channels.remove(channel);
                inner.unsubscribe(&self.object, channel, self.id);
            }
            _ => return false,
        }
        true
    }
}

impl Sockets {
    /// Sockets for an app that may hold `max` of them.
    pub fn new(max: usize) -> Sockets {
        Sockets { inner: Mutex::default(), max }
    }

    /// Opens a socket on `object` if the caps allow it and the object accepts:
    /// the 101 to answer the upgrade with. `req` has had the app headers
    /// removed; its own WebSocket headers are the client's.
    pub async fn open(
        self: &Arc<Self>,
        object: &str,
        mut req: Request<Body>,
        caps: Caps<'_>,
        gone: watch::Receiver<()>,
        deliver: Deliver,
    ) -> Result<Response<()>, (u16, &'static str)> {
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

        if caps.total.fetch_add(1, Relaxed) >= caps.max_total {
            caps.total.fetch_sub(1, Relaxed);
            return Err((503, "the node has max_sockets open"));
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let out = Out {
            tx,
            queued: Default::default(),
            max: caps.backlog,
            behind: Default::default(),
        };
        let id = NEXT.fetch_add(1, Relaxed);
        let mut conn = Conn {
            id,
            object: object.to_string(),
            uri: req.uri().clone(),
            headers,
            grip: false,
            sockets: self.clone(),
            out: out.clone(),
            total: caps.total.clone(),
        };
        {
            let mut inner = self.inner.lock();
            if inner.socks.len() >= self.max {
                return Err((503, "the app has max_sockets open"));
            }
            if inner.objects.get(object).is_some_and(|n| *n >= caps.per_object) {
                return Err((503, "the object has max_sockets open"));
            }
            *inner.objects.entry(object.to_string()).or_default() += 1;
            inner.socks.insert(id, Sock { channels: HashSet::new(), out });
        }

        let mut open = conn.request(&Event::Open);
        open.headers_mut().insert(SEC_WEBSOCKET_EXTENSIONS, HeaderValue::from_static("grip"));
        let (headers, events) = match deliver(open).await {
            Reply::Answer(Answer { headers, events }) => (headers, events),
            Reply::Bad(status) => return Err((status, "the object refused the socket")),
            Reply::Gone => return Err((503, "the app's worker is restarting")),
        };
        conn.grip = headers.get(SEC_WEBSOCKET_EXTENSIONS).is_some_and(|e| e == "grip");
        let protocol = headers.get(PROTOCOL);
        let offered = |p: &HeaderValue| {
            let offers = req.headers().get_all(PROTOCOL).iter().filter_map(|v| v.to_str().ok());
            offers.flat_map(|v| v.split(',')).any(|o| o.trim().as_bytes() == p.as_bytes())
        };
        let mut events = events.into_iter();
        if events.next() != Some(Event::Open) || !protocol.is_none_or(offered) || !conn.apply(events.collect()) {
            return Err((502, "the object did not accept the socket"));
        }

        let mut res = Response::builder()
            .status(101)
            .header(http::header::CONNECTION, "upgrade")
            .header(http::header::UPGRADE, "websocket")
            .header(http::header::SEC_WEBSOCKET_ACCEPT, accept);
        if let Some(p) = protocol {
            res = res.header(PROTOCOL, p);
        }
        let config = WebSocketConfig::default()
            .max_message_size(Some(caps.request_max.saturating_sub(FRAMING)))
            .max_frame_size(Some(caps.request_max.saturating_sub(FRAMING)));
        tokio::spawn(async move {
            match upgrade.await {
                Ok(io) => run(conn, hyper_util::rt::TokioIo::new(io), rx, gone, config, deliver).await,
                Err(_) => drop(deliver(conn.request(&Event::Disconnect)).await),
            }
        });
        Ok(res.body(()).expect("a valid response"))
    }

    /// Sends what `object` publishes to its sockets subscribed to each item's
    /// channel: GRIP's `{"items": [{"channel", "formats": {"ws-message":
    /// {"content" | "content-bin"}}}]}`. The status to answer with.
    pub async fn publish<B>(&self, req: Request<B>, max: usize) -> u16
    where
        B: http_body::Body<Data = Bytes>,
        B::Error: Into<crate::object::BoxError>,
    {
        let object = req.headers().get(OBJECT_HEADER).and_then(|v| v.to_str().ok()).map(str::to_string);
        let Ok(body) = Limited::new(req.into_body(), max).collect().await else { return 413 };
        let (Some(object), Some(items)) = (object, messages(&body.to_bytes())) else { return 400 };
        let inner = self.inner.lock();
        for (channel, m) in items {
            for id in inner.channels.get(&(object.clone(), channel)).into_iter().flatten() {
                inner.socks[id].out.send(m.clone());
            }
        }
        200
    }
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
    mut gone: watch::Receiver<()>,
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
                Reply::Gone => break (frame(CloseCode::Restart), None),
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
            _ = gone.changed() => break (frame(CloseCode::Restart), None),
            // A client silent through a whole interval (while nothing waits
            // on the object) is gone, even if its connection never said so.
            _ = ping.tick() => {
                if !heard && pending.is_terminated() {
                    break (None, Some(Event::Disconnect));
                }
                heard = false;
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
        let sockets = Arc::new(Sockets::new(1));
        let (tx, rx) = mpsc::unbounded_channel();
        let out = Out { tx, queued: Default::default(), max: 1 << 20, behind: Default::default() };
        let total = Arc::new(AtomicUsize::new(1));
        let conn = Conn {
            id: 1,
            object: "o".into(),
            uri: "/a/o/".parse().unwrap(),
            headers: HeaderMap::new(),
            grip: false,
            sockets,
            out,
            total: total.clone(),
        };
        let told = Arc::new(Mutex::new(vec![]));
        let t = told.clone();
        let deliver: Deliver = Box::new(move |req| {
            t.lock().push(req.into_body());
            Box::pin(async { Reply::Gone })
        });
        let (_client, server) = tokio::io::duplex(1 << 16);
        let (_gone_tx, gone) = watch::channel(());
        let ran = tokio::spawn(run(conn, server, rx, gone, WebSocketConfig::default(), deliver));
        tokio::time::sleep(PING * 2 + Duration::from_secs(1)).await;
        assert!(ran.is_finished(), "a silent client is still open");
        assert_eq!(told.lock().len(), 1, "the object is told");
        assert_eq!(total.load(Relaxed), 0);
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
}
