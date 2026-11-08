//! WebSockets. nanovm's proxy holds each client's socket and turns what
//! happens on it into ordinary requests to the object
//! ([Pushpin's WebSocket-over-HTTP](https://pushpin.org/docs/protocols/websocket-over-http/)),
//! queued with the object's other requests, so a socket's messages arrive in
//! order with everything else the object serves. The object answers each
//! with events for that socket ([`Answer`]), and reaches every socket
//! subscribed to one of its channels, from any request, with [`publish`].
//! Sockets outlive instances: an object can be evicted and restarted under
//! its open sockets.
//!
//! ```no_run
//! # use nanovm_sdk::{Request, Result, ws};
//! async fn socket(events: Vec<ws::Event>) -> Result {
//!     let mut answer = ws::Answer::default();
//!     for event in events {
//!         match event {
//!             ws::Event::Open => {
//!                 answer.accept().subscribe("room");
//!             }
//!             ws::Event::Text(text) => {
//!                 ws::publish("room", &text).await?;
//!             }
//!             _ => {}
//!         }
//!     }
//!     Ok(answer.into())
//! }
//! ```

use wasip3::http::types::ErrorCode;

use crate::{Request, Response, quote};

/// The content type of a socket's requests and their answers.
pub const CONTENT_TYPE: &str = "application/websocket-events";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A client asks to connect; [`Answer::accept`] lets it.
    Open,
    Text(String),
    Binary(Vec<u8>),
    /// The client closed the socket, with this code.
    Close(Option<u16>),
    /// The socket ended some other way.
    Disconnect,
}

/// The events `req` carries, if it is one of a socket's requests. Only the
/// proxy can send those, so its `connection-id` header can be trusted.
pub fn events(req: &Request) -> Option<Vec<Event>> {
    if req.header("content-type") != Some(CONTENT_TYPE) {
        return None;
    }
    // `KIND\r\n`, or `KIND HEXLEN\r\n` then that much content and `\r\n`.
    let (mut body, mut events) = (req.body(), vec![]);
    while let Some(end) = body.windows(2).position(|w| w == b"\r\n") {
        let head = String::from_utf8_lossy(&body[..end]).into_owned();
        body = &body[end + 2..];
        let (kind, content) = match head.split_once(' ') {
            Some((kind, n)) => {
                let n = usize::from_str_radix(n, 16).unwrap_or(0).min(body.len());
                let content = body[..n].to_vec();
                body = &body[(n + 2).min(body.len())..];
                (kind, content)
            }
            None => (head.as_str(), vec![]),
        };
        events.push(match kind {
            "OPEN" => Event::Open,
            "TEXT" => Event::Text(String::from_utf8_lossy(&content).into_owned()),
            "BINARY" => Event::Binary(content),
            "CLOSE" => Event::Close(content.try_into().ok().map(u16::from_be_bytes)),
            _ => Event::Disconnect,
        });
    }
    Some(events)
}

/// What to do on the socket that sent a request; `.into()` the [`Response`].
#[derive(Default)]
pub struct Answer(Vec<u8>);

impl Answer {
    /// Accepts the socket. Answer `OPEN` without it and the socket is refused.
    pub fn accept(&mut self) -> &mut Answer {
        self.0.extend(b"OPEN\r\n");
        self
    }

    /// Subscribes the socket to `channel`, of this object, for [`publish`].
    pub fn subscribe(&mut self, channel: &str) -> &mut Answer {
        self.control("subscribe", channel)
    }

    pub fn unsubscribe(&mut self, channel: &str) -> &mut Answer {
        self.control("unsubscribe", channel)
    }

    /// Sends `text` to the socket.
    pub fn text(&mut self, text: &str) -> &mut Answer {
        self.event("TEXT", format!("m:{text}").as_bytes())
    }

    pub fn binary(&mut self, data: &[u8]) -> &mut Answer {
        self.event("BINARY", data)
    }

    /// Closes the socket with `code`.
    pub fn close(&mut self, code: u16) -> &mut Answer {
        self.event("CLOSE", &code.to_be_bytes())
    }

    fn control(&mut self, kind: &str, channel: &str) -> &mut Answer {
        self.event("TEXT", format!(r#"c:{{"type":"{kind}","channel":{}}}"#, quote(channel)).as_bytes())
    }

    fn event(&mut self, kind: &str, content: &[u8]) -> &mut Answer {
        self.0.extend(format!("{kind} {:x}\r\n", content.len()).as_bytes());
        self.0.extend(content);
        self.0.extend(b"\r\n");
        self
    }
}

impl From<Answer> for Response {
    fn from(answer: Answer) -> Response {
        // GRIP: text is `m:` messages and `c:` controls, as `Answer` writes it.
        Response::bytes(CONTENT_TYPE, answer.0).header("sec-websocket-extensions", "grip")
    }
}

/// Sends `message` to every socket of this object subscribed to `channel`,
/// once this request commits (never, if it fails). The node's status: `200`.
pub async fn publish(channel: &str, message: &str) -> Result<u16, ErrorCode> {
    publish_all(channel, [message]).await
}

/// [`publish`] for several messages (at most 64) in one go.
pub async fn publish_all<M: AsRef<str>>(channel: &str, messages: impl IntoIterator<Item = M>) -> Result<u16, ErrorCode> {
    let item = |m: M| format!(r#"{{"channel":{},"formats":{{"ws-message":{{"content":{}}}}}}}"#, quote(channel), quote(m.as_ref()));
    let body = format!(r#"{{"items":[{}]}}"#, messages.into_iter().map(item).collect::<Vec<_>>().join(","));
    Ok(crate::fetch("http://nanovm/publish/").method("POST").body(body).send().await?.status)
}
