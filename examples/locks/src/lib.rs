//! A lock and lease service: each lock is an object (`/{lock}/`), so it
//! has exactly one writer and every acquire, renew and release is decided in
//! order. A lease expires on its own after `ttl_ms`, and every grant comes
//! with a fencing token, larger than any before it, for the protected
//! resource to check: a holder that stalled past its lease and comes back
//! carries an older token than the new holder's, and is refused.
//!
//! - `POST /{lock}/acquire?owner=…&ttl_ms=10000`: `200` with the lease, or
//!   `409` with the current holder's. Its holder acquiring again renews it.
//! - `POST /{lock}/renew?owner=…&token=…&ttl_ms=…`: `200`, or `409` if the
//!   lease is not that holder's any more
//! - `POST /{lock}/release?owner=…&token=…`: `204`, or `409`
//! - `GET /{lock}/`: the lease, if one is held

use std::time::{SystemTime, UNIX_EPOCH};

use nanovm_sdk::http::header::CONTENT_TYPE;
use nanovm_sdk::http::{Request, Response};
use nanovm_sdk::{Body, RequestExt, Result, kv, serde_json};
use serde::{Deserialize, Serialize};

nanovm_sdk::serve!(handle);

#[derive(Serialize, Deserialize)]
struct Lease {
    owner: String,
    token: i64,
    expires_at: u64,
}

async fn handle(req: Request<Body>) -> Result {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let lease = match kv::get("lease").await? {
        Some(json) => Some(serde_json::from_slice::<Lease>(&json)?).filter(|l| l.expires_at > now),
        None => None,
    };
    let owner = req.query("owner").unwrap_or_default();
    let ttl = req.query("ttl_ms").as_deref().unwrap_or("10000").parse::<u64>().unwrap_or(0);
    if req.method() == "POST" && (!(1..=64).contains(&owner.len()) || !(1..=3_600_000).contains(&ttl)) {
        return text(400, "owner is 1-64 bytes, ttl_ms 1-3600000\n");
    }
    // The lease named by `owner` and `token`, if it is still theirs.
    let theirs = |l: &Lease| l.owner == owner && Some(l.token.to_string()) == req.query("token");
    Ok(match (req.method().as_str(), req.uri().path(), lease) {
        ("GET", "/", lease) => json(200, serde_json::to_string(&lease)?)?,
        ("POST", "/acquire", Some(l)) if l.owner != owner => conflict(&l)?,
        ("POST", "/acquire", held) => {
            // A new grant gets a new token; its holder acquiring again keeps theirs.
            let token = match held {
                Some(l) => l.token,
                None => kv::increment("fence", 1).await?,
            };
            grant(Lease { owner: owner.clone(), token, expires_at: now + ttl }).await?
        }
        ("POST", "/renew", Some(l)) if theirs(&l) => grant(Lease { expires_at: now + ttl, ..l }).await?,
        ("POST", "/release", Some(l)) if theirs(&l) => {
            kv::delete("lease").await?;
            Response::builder().status(204).body(Body::empty())?
        }
        ("POST", "/renew" | "/release", lease) => match lease {
            Some(l) => conflict(&l)?,
            None => json(409, "null")?,
        },
        _ => text(404, "not found")?,
    })
}

async fn grant(lease: Lease) -> Result {
    let lease = serde_json::to_string(&lease)?;
    kv::set("lease", lease.as_str()).await?;
    json(200, lease)
}

fn conflict(holder: &Lease) -> Result {
    json(409, serde_json::to_string(holder)?)
}

/// A JSON answer.
fn json(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "application/json").body(body.into())?)
}

/// A plain-text answer.
fn text(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "text/plain; charset=utf-8").body(body.into())?)
}
