//! File uploads and downloads with HTTP range requests. Each bucket is an
//! object (`/{bucket}/`); its files are in its volume, so they count
//! against the app's `disk_mb`, and an upload over it is refused with `507`.
//!
//! - `PUT /{bucket}/files/{name}` (body: the file, at most `max_request_body_kb`)
//! - `GET /{bucket}/files/{name}`: the file, streamed; with `Range: bytes=…`
//!   the part asked for (`206`); with `If-None-Match` its `ETag`, `304`
//! - `HEAD` and `DELETE` of the same; `GET /{bucket}/`: the files

use std::hash::{Hash, Hasher};

use nanovm_sdk::fs::{self, DescriptorFlags, OpenFlags};
use nanovm_sdk::{Request, Response, Result, data, kv, serde_json};
use serde::{Deserialize, Serialize};

nanovm_sdk::serve!(handle);

#[derive(Serialize, Deserialize)]
struct Meta {
    name: String,
    size: u64,
    etag: String,
    #[serde(rename = "type")]
    content_type: String,
}

const UPLOAD: &str = ".upload";
const PIECE: u64 = 64 << 10;

async fn handle(req: Request) -> Result {
    let path: Vec<&str> = req.path().trim_matches('/').split('/').collect();
    let file = match path.as_slice() {
        ["files", name] if valid(name) => name.to_string(),
        [""] if req.method() == "GET" => return list().await,
        _ => return Ok(Response::not_found()),
    };
    let file = file.as_str();
    let meta: Option<Meta> = match kv::get(file).await? {
        Some(json) => Some(serde_json::from_slice(&json)?),
        None => None,
    };
    Ok(match (req.method(), meta) {
        ("PUT", _) => upload(file, req).await?,
        ("GET" | "HEAD", Some(meta)) => download(&req, meta).await?,
        ("DELETE", Some(_)) => {
            data().remove(&format!("f-{file}")).await?;
            kv::delete(file).await?;
            Response::new(204)
        }
        _ => Response::not_found(),
    })
}

async fn upload(name: &str, req: Request) -> Result {
    let content_type = req.header("content-type").unwrap_or("application/octet-stream").to_string();
    let body = req.into_body();
    let mut hasher = std::hash::DefaultHasher::new();
    body.hash(&mut hasher);
    let meta = Meta { name: name.into(), size: body.len() as u64, etag: format!("\"{:016x}\"", hasher.finish()), content_type };
    // Written aside, then renamed over the old file: if the disk is full,
    // the old file is untouched and this answer (which commits) says so.
    // (A failure that returned an error would roll back anyway, but would
    // answer 500.)
    if let Err(e) = data().write(UPLOAD, body).await {
        let _ = data().remove(UPLOAD).await;
        return Ok(match fs::code(&e).as_str() {
            "insufficient-space" | "quota" => Response::text(format!("over this bucket's disk quota ({})\n", fs::code(&e))).status(507),
            _ => return Err(e.into()),
        });
    }
    data().rename(UPLOAD, &format!("f-{name}")).await?;
    let json = serde_json::to_string(&meta)?;
    kv::set(name, json.as_str()).await?;
    Ok(Response::json(json).status(201).header("etag", &meta.etag))
}

async fn download(req: &Request, meta: Meta) -> Result {
    if req.header("if-none-match") == Some(&meta.etag) {
        return Ok(Response::new(304).header("etag", &meta.etag));
    }
    let size = meta.size;
    let ((start, end), status) = match req.header("range").map(|r| range(r, size)) {
        Some(Some(Ok(r))) => (r, 206),
        Some(Some(Err(()))) => {
            return Ok(Response::new(416).header("content-range", format!("bytes */{size}")));
        }
        // No range, or one we don't serve (several): the whole file.
        _ => ((0, size), 200),
    };
    let fd = data().open(&format!("f-{}", meta.name), OpenFlags::empty(), DescriptorFlags::READ).await?;
    let head = req.method() == "HEAD";
    // Streamed a piece at a time, so a large file never sits in memory.
    let mut res = Response::stream(&meta.content_type, move |mut body| async move {
        let mut at = start;
        while at < end && !head {
            match fs::read_at(&fd, at, (end - at).min(PIECE) as usize).await {
                Ok(piece) if !piece.is_empty() => {
                    at += piece.len() as u64;
                    if !body.write(piece).await {
                        return;
                    }
                }
                _ => return,
            }
        }
    })
    .status(status)
    .header("accept-ranges", "bytes")
    .header("etag", &meta.etag)
    .header("content-length", (end - start).to_string());
    if status == 206 {
        res = res.header("content-range", format!("bytes {start}-{}/{size}", end - 1));
    }
    Ok(res)
}

/// One range of `Range: bytes=…` as `[start, end)`: `None` if it isn't one
/// (several ranges, other units), `Err` if it can't be satisfied.
fn range(header: &str, size: u64) -> Option<core::result::Result<(u64, u64), ()>> {
    let spec = header.strip_prefix("bytes=")?.trim();
    let (a, b) = spec.split_once('-').filter(|_| !spec.contains(','))?;
    let (start, end) = match (a.parse::<u64>().ok(), b.parse::<u64>().ok()) {
        (Some(a), Some(b)) if a <= b => (a, b.saturating_add(1).min(size)),
        (Some(a), None) if b.is_empty() => (a, size),
        (None, Some(n)) if a.is_empty() => (size.saturating_sub(n), size),
        _ => return None,
    };
    Some(if start < end { Ok((start, end)) } else { Err(()) })
}

async fn list() -> Result {
    let mut files = vec![];
    for key in kv::keys().await? {
        if let Some(json) = kv::get(&key).await? {
            files.push(serde_json::from_slice::<Meta>(&json)?);
        }
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    let bytes: u64 = files.iter().map(|f| f.size).sum();
    Ok(Response::json(serde_json::json!({ "files": files, "bytes": bytes }).to_string()))
}

/// A file name: 1-100 letters, digits, `.`, `_` or `-`, not starting with `.`.
fn valid(name: &str) -> bool {
    (1..=100).contains(&name.len()) && !name.starts_with('.') && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
