//! Echo: answers a request with what the app saw of it, to check what
//! reaches an app and what leaves it.
//!
//! - `{any method} /{object}/…?status=N`: an answer of status `N` (200 if
//!   there is none) with the request's body and `content-type`, every
//!   `x-echo-*` header it came with, as sent (each of a repeated one, and
//!   bytes that aren't UTF-8), and `x-seen`: its method and URI as the app
//!   saw them
//! - `/{object}/relay/{other}/…`: the same request sent on to object
//!   `{other}` as `/…`, by a call between objects, and its answer passed
//!   back as it came

use nanovm_sdk::http::header::{CONTENT_TYPE, HeaderName};
use nanovm_sdk::http::{Request, Response};
use nanovm_sdk::{Body, RequestExt, Result, object};

nanovm_sdk::serve!(handle);

async fn handle(req: Request<Body>) -> Result {
    let echoed = |name: &HeaderName| name.as_str().starts_with("x-echo-") || name == CONTENT_TYPE;
    let headers = req.headers().iter().filter(|(name, _)| echoed(name));
    if let Some(rest) = req.uri().path().strip_prefix("/relay/") {
        let (other, path) = rest.split_once('/').unwrap_or((rest, ""));
        let query = req.uri().query().map_or(String::new(), |q| format!("?{q}"));
        let relay = Request::builder().method(req.method()).uri(format!("/{path}{query}"));
        let relay = headers.fold(relay, |r, (name, value)| r.header(name, value));
        return Ok(object(other).fetch(relay.body(req.into_body())?).await?);
    }
    let seen = format!("{} {}", req.method(), req.uri());
    let answer = Response::builder().status(req.query_or("status", 200u16)).header("x-seen", seen);
    let answer = headers.fold(answer, |r, (name, value)| r.header(name, value));
    Ok(answer.body(req.into_body())?)
}
