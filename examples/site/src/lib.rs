//! A static site and its API in one component. The site's files are
//! embedded in the app at build time and served with an `ETag` (a hash of
//! their bytes) and `Cache-Control`; the API keeps its state in the object's
//! key-value store. Each object serves the site at its own path, so the site
//! is usually one object, `/www/`.
//!
//! - `GET /{object}/` and `GET /{object}/{file}`: the embedded files
//! - `GET /{object}/api/likes`, `POST /{object}/api/likes`: `{"likes":n}`

use std::hash::{Hash, Hasher};

use nanovm_sdk::{Request, Response, Result, kv};

nanovm_sdk::serve!(handle);

/// Path, content type, `Cache-Control`, bytes.
const FILES: &[(&str, &str, &str, &[u8])] = &[
    // The page names the other files, so it is checked on every visit
    // (cheaply: an unchanged page is a 304); the rest may be kept an hour.
    ("/", "text/html; charset=utf-8", "no-cache", include_bytes!("public/index.html")),
    ("/style.css", "text/css; charset=utf-8", "public, max-age=3600", include_bytes!("public/style.css")),
    ("/app.js", "text/javascript; charset=utf-8", "public, max-age=3600", include_bytes!("public/app.js")),
];

async fn handle(req: Request) -> Result {
    match (req.method(), req.path()) {
        (_, "/api/likes") => {
            let likes = match req.method() {
                "POST" => kv::increment("likes", 1).await?,
                _ => kv::get_text("likes").await?.and_then(|n| n.parse().ok()).unwrap_or(0),
            };
            return Ok(Response::json(format!(r#"{{"likes":{likes}}}"#)).header("cache-control", "no-store"));
        }
        ("GET" | "HEAD", path) => {
            if let Some((_, content_type, cache, bytes)) = FILES.iter().find(|f| f.0 == path) {
                return Ok(file(&req, content_type, cache, bytes));
            }
        }
        _ => {}
    }
    Ok(Response::not_found())
}

fn file(req: &Request, content_type: &str, cache: &str, bytes: &[u8]) -> Response {
    let mut hasher = std::hash::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let etag = format!("\"{:016x}\"", hasher.finish());
    let response = match req.header("if-none-match") {
        Some(tags) if tags.split(',').any(|t| t.trim() == etag || t.trim() == "*") => Response::new(304),
        // A HEAD gets a GET's headers, its length included, and no body.
        _ if req.method() == "HEAD" => {
            Response::stream(content_type, |_| async {}).header("content-length", bytes.len().to_string())
        }
        _ => Response::bytes(content_type, bytes),
    };
    response.header("etag", &etag).header("cache-control", cache)
}
