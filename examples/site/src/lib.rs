//! A static site and its API in one component. The site's files are
//! embedded in the app at build time and served with an `ETag` (a hash of
//! their bytes) and `Cache-Control`; the API keeps its state in the object's
//! key-value store. Each object serves the site at its own path, so the site
//! is usually one object, `/www/`.
//!
//! - `GET /{object}/` and `GET /{object}/{file}`: the embedded files
//! - `GET /{object}/api/likes`, `POST /{object}/api/likes`: `{"likes":n}`

use std::hash::{Hash, Hasher};

use nanovm_sdk::http::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use nanovm_sdk::http::{Request, Response};
use nanovm_sdk::{Body, Result, kv};

nanovm_sdk::serve!(handle);

/// Path, content type, `Cache-Control`, bytes.
const FILES: &[(&str, &str, &str, &[u8])] = &[
    // The page names the other files, so it is checked on every visit
    // (cheaply: an unchanged page is a 304); the rest may be kept an hour.
    ("/", "text/html; charset=utf-8", "no-cache", include_bytes!("public/index.html")),
    ("/style.css", "text/css; charset=utf-8", "public, max-age=3600", include_bytes!("public/style.css")),
    ("/app.js", "text/javascript; charset=utf-8", "public, max-age=3600", include_bytes!("public/app.js")),
];

async fn handle(req: Request<Body>) -> Result {
    match (req.method().as_str(), req.uri().path()) {
        (_, "/api/likes") => {
            let likes = match req.method().as_str() {
                "POST" => kv::increment("likes", 1).await?,
                _ => kv::get_text("likes").await?.and_then(|n| n.parse().ok()).unwrap_or(0),
            };
            let json = Response::builder().header(CONTENT_TYPE, "application/json").header(CACHE_CONTROL, "no-store");
            return Ok(json.body(format!(r#"{{"likes":{likes}}}"#).into())?);
        }
        ("GET" | "HEAD", path) => {
            if let Some((_, content_type, cache, bytes)) = FILES.iter().find(|f| f.0 == path) {
                return file(&req, content_type, cache, bytes);
            }
        }
        _ => {}
    }
    text(404, "not found")
}

fn file(req: &Request<Body>, content_type: &str, cache: &str, bytes: &[u8]) -> Result {
    let mut hasher = std::hash::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let etag = format!("\"{:016x}\"", hasher.finish());
    let response = Response::builder().header(ETAG, &etag).header(CACHE_CONTROL, cache);
    let tags = req.headers().get(IF_NONE_MATCH).and_then(|v| v.to_str().ok());
    Ok(match tags {
        Some(tags) if tags.split(',').any(|t| t.trim() == etag || t.trim() == "*") => response.status(304).body(Body::empty())?,
        // A HEAD gets a GET's headers, its length included, and no body.
        _ if req.method() == "HEAD" => {
            let head = response.header(CONTENT_TYPE, content_type).header(CONTENT_LENGTH, bytes.len());
            head.body(Body::stream(|_| async { Ok(()) }))?
        }
        _ => response.header(CONTENT_TYPE, content_type).body(bytes.into())?,
    })
}

/// A plain-text answer.
fn text(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "text/plain; charset=utf-8").body(body.into())?)
}
