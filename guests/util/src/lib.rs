//! Small helpers shared by the nanovm guest components: request parsing,
//! response building and wasi:filesystem@0.3 file I/O.

use std::collections::HashMap;

use wasip3::filesystem::preopens;
use wasip3::filesystem::types::{Descriptor, DescriptorFlags, ErrorCode as FsError, OpenFlags, PathFlags};
use wasip3::http::types::{ErrorCode, Fields, Method, Request, Response};
use wasip3::{wit_future, wit_stream};

pub use wasip3;

/// `wasi:keyvalue` (store, atomics, batch), as nanovm offers it.
pub mod kv {
    wit_bindgen::generate!({ path: "../../wit/keyvalue", world: "wasi:keyvalue/imports", generate_all });
    pub use wasi::keyvalue::*;
}

/// `wasi:config/store`: the app's config values.
pub mod config {
    wit_bindgen::generate!({ path: "../../wit/config", world: "wasi:config/imports", generate_all });
    pub use wasi::config::store::*;
}

/// A fully-read incoming request.
pub struct Req {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Req {
    pub fn q(&self, key: &str) -> Option<&str> {
        self.query.get(key).map(|s| s.as_str())
    }

    pub fn q_u64(&self, key: &str, default: u64) -> u64 {
        self.q(key).and_then(|v| v.parse().ok()).unwrap_or(default)
    }
}

pub async fn read_req(request: Request) -> Req {
    let method = match request.get_method() {
        Method::Get => "GET".to_string(),
        Method::Head => "HEAD".to_string(),
        Method::Post => "POST".to_string(),
        Method::Put => "PUT".to_string(),
        Method::Delete => "DELETE".to_string(),
        Method::Patch => "PATCH".to_string(),
        Method::Options => "OPTIONS".to_string(),
        Method::Connect => "CONNECT".to_string(),
        Method::Trace => "TRACE".to_string(),
        Method::Other(m) => m,
    };
    let pq = request.get_path_with_query().unwrap_or_else(|| "/".to_string());
    let (path, qs) = match pq.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (pq, String::new()),
    };
    let query = qs
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(kv), String::new()),
        })
        .collect();
    let (result_tx, result_rx) = wit_future::new(|| Ok(()));
    let (body_rx, _trailers) = Request::consume_body(request, result_rx);
    let body = body_rx.collect().await;
    drop(result_tx);
    Req {
        method,
        path,
        query,
        body,
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Builds a response whose body is written by a spawned task after `handle`
/// returns (writing before returning would deadlock: the host only starts
/// reading the body once it has the response).
pub fn respond(status: u16, content_type: &str, body: Vec<u8>) -> Result<Response, ErrorCode> {
    let headers = Fields::from_list(&[
        ("content-type".to_string(), content_type.as_bytes().to_vec()),
        ("content-length".to_string(), body.len().to_string().into_bytes()),
    ])
    .map_err(|e| ErrorCode::InternalError(Some(format!("{e:?}"))))?;
    let (mut body_tx, body_rx) = wit_stream::new();
    let (trailers_tx, trailers_rx) = wit_future::new(|| Ok(None));
    let (response, _done) = Response::new(headers, Some(body_rx), trailers_rx);
    drop(trailers_tx);
    let _ = response.set_status_code(status);
    wasip3::spawn_local(async move {
        let _ = body_tx.write_all(body).await;
    });
    Ok(response)
}

pub fn text(status: u16, body: impl Into<String>) -> Result<Response, ErrorCode> {
    respond(status, "text/plain; charset=utf-8", body.into().into_bytes())
}

pub fn json(status: u16, body: impl Into<String>) -> Result<Response, ErrorCode> {
    respond(status, "application/json", body.into().into_bytes())
}

/// Minimal JSON string escaping.
pub fn js(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

// --- filesystem (wasi:filesystem@0.3) ---------------------------------------

thread_local! {
    // Preopens are fetched once per instance and leaked so callers can hold
    // `&'static Descriptor` across awaits.
    static MOUNTS: &'static [(Descriptor, String)] =
        Box::leak(preopens::get_directories().into_boxed_slice());
}

/// The preopened directory mounted at `guest_path` (`/app`, `/tmp`, `/data`).
pub fn mount(guest_path: &str) -> Option<&'static Descriptor> {
    MOUNTS.with(|m| m.iter().find(|(_, p)| p == guest_path).map(|(d, _)| d))
}

/// Opens `path` under `mount`. Returns a fresh descriptor the caller owns.
pub async fn open(
    mount_path: &str,
    path: &str,
    oflags: OpenFlags,
    flags: DescriptorFlags,
) -> Result<Descriptor, FsError> {
    let dir = mount(mount_path).ok_or(FsError::NoEntry)?;
    dir.open_at(PathFlags::empty(), path.to_string(), oflags, flags).await
}

pub async fn write_file(mount: &str, path: &str, data: Vec<u8>) -> Result<(), FsError> {
    let fd = open(
        mount,
        path,
        OpenFlags::CREATE | OpenFlags::TRUNCATE,
        DescriptorFlags::READ | DescriptorFlags::WRITE,
    )
    .await?;
    write_all(&fd, data, 0).await
}

/// Replaces `path` atomically: writes a temporary sibling, then renames it
/// over `path`. A kill mid-write leaves the old contents (and at worst a
/// stray `.tmp` file), never a torn or empty file.
pub async fn write_file_atomic(mount_path: &str, path: &str, data: Vec<u8>) -> Result<(), FsError> {
    let tmp = format!("{path}.tmp");
    write_file(mount_path, &tmp, data).await?;
    let dir = mount(mount_path).ok_or(FsError::NoEntry)?;
    dir.rename_at(tmp, dir, path.to_string()).await
}

pub async fn append_file(mount: &str, path: &str, data: Vec<u8>) -> Result<(), FsError> {
    let fd = open(
        mount,
        path,
        OpenFlags::CREATE,
        DescriptorFlags::READ | DescriptorFlags::WRITE,
    )
    .await?;
    let (mut tx, rx) = wit_stream::new();
    let done = fd.append_via_stream(rx);
    wasip3::spawn_local(async move {
        let _ = tx.write_all(data).await;
    });
    done.await
}

/// Writes `data` to `fd` at `offset` through `write-via-stream`.
pub async fn write_all(fd: &Descriptor, data: Vec<u8>, offset: u64) -> Result<(), FsError> {
    let (mut tx, rx) = wit_stream::new();
    let done = fd.write_via_stream(rx, offset);
    wasip3::spawn_local(async move {
        let _ = tx.write_all(data).await;
    });
    done.await
}

pub async fn read_file(mount: &str, path: &str) -> Result<Vec<u8>, FsError> {
    let fd = open(mount, path, OpenFlags::empty(), DescriptorFlags::READ).await?;
    let (rx, done) = fd.read_via_stream(0);
    let data = rx.collect().await;
    done.await?;
    Ok(data)
}

pub fn fs_err(e: &FsError) -> String {
    // Debug prints `ErrorCode::InsufficientSpace`; keep the variant and turn
    // CamelCase into kebab-case to match the WIT names.
    let s = format!("{e:?}");
    let s = s.rsplit("::").next().unwrap_or(&s);
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            break;
        }
    }
    out
}

pub use wasip3::filesystem::types::{
    Descriptor as Fd, DescriptorFlags as FdFlags, ErrorCode as FsErrorCode, OpenFlags as OFlags,
};
