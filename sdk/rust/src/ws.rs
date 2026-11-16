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
//! # use nanovm_sdk::{Result, ws};
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

use http::header::{CONTENT_TYPE as CONTENT_TYPE_HEADER, HeaderValue};
use http::{Request, Response, StatusCode};
use wasip3::http::types::ErrorCode;

use crate::{Body, quote};

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
pub fn events(req: &Request<Body>) -> Option<Vec<Event>> {
    if req.headers().get(CONTENT_TYPE_HEADER)? != CONTENT_TYPE {
        return None;
    }
    // `KIND\r\n`, or `KIND HEXLEN\r\n` then that much content and `\r\n`.
    let (mut body, mut events) = (req.body().as_bytes(), vec![]);
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

/// What to do on the socket that sent a request; `.into()` the response.
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

impl From<Answer> for Response<Body> {
    fn from(answer: Answer) -> Response<Body> {
        let mut response = Response::new(answer.0.into());
        response.headers_mut().insert(CONTENT_TYPE_HEADER, HeaderValue::from_static(CONTENT_TYPE));
        // GRIP: text is `m:` messages and `c:` controls, as `Answer` writes it.
        response.headers_mut().insert("sec-websocket-extensions", HeaderValue::from_static("grip"));
        response
    }
}

/// Sends `message` to every socket of this object subscribed to `channel`,
/// once this request commits (never, if it fails). The node's status: `200`.
pub async fn publish(channel: &str, message: &str) -> Result<StatusCode, ErrorCode> {
    publish_all(channel, [message]).await
}

/// [`publish`] for several messages (at most 64) in one go.
pub async fn publish_all<M: AsRef<str>>(channel: &str, messages: impl IntoIterator<Item = M>) -> Result<StatusCode, ErrorCode> {
    let item = |m: M| format!(r#"{{"channel":{},"formats":{{"ws-message":{{"content":{}}}}}}}"#, quote(channel), quote(m.as_ref()));
    let body = format!(r#"{{"items":[{}]}}"#, messages.into_iter().map(item).collect::<Vec<_>>().join(","));
    Ok(crate::fetch(Request::post("http://nanovm/publish/").body(body).expect("a valid request")).await?.status())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(content_type: &str, body: &[u8]) -> Request<Body> {
        Request::post("/ws").header("content-type", content_type).body(body.into()).unwrap()
    }

    #[test]
    fn a_sockets_requests_carry_its_events() {
        let body = b"OPEN\r\nTEXT 2\r\nhi\r\nBINARY 3\r\n\x00\xff\r\r\nCLOSE 2\r\n\x03\xe8\r\nDISCONNECT\r\n";
        let got = events(&request(CONTENT_TYPE, body)).unwrap();
        let close = Event::Close(Some(1000));
        assert_eq!(got, [Event::Open, Event::Text("hi".into()), Event::Binary(vec![0, 0xff, b'\r']), close, Event::Disconnect]);
        assert_eq!(events(&request("text/plain", body)), None, "other requests are not a socket's");
        assert_eq!(events(&Request::new(Body::empty())), None);
        assert_eq!(events(&request(CONTENT_TYPE, b"TEXT 9\r\nab")).unwrap(), [Event::Text("ab".into())], "a cut event ends");
    }

    #[test]
    fn an_answer_is_a_grip_response() {
        let mut answer = Answer::default();
        answer.accept().subscribe("room").text("hi").close(1000);
        let response: Response<Body> = answer.into();
        let header = |name| response.headers().get(name).unwrap().to_str().unwrap();
        assert_eq!((response.status(), header("content-type"), header("sec-websocket-extensions")), (StatusCode::OK, CONTENT_TYPE, "grip"));
        let control = br#"c:{"type":"subscribe","channel":"room"}"#;
        let body = [&b"OPEN\r\nTEXT 27\r\n"[..], control, b"\r\nTEXT 4\r\nm:hi\r\nCLOSE 2\r\n\x03\xe8\r\n"].concat();
        assert_eq!((control.len(), response.body().as_bytes()), (0x27, &body[..]));
    }
}
