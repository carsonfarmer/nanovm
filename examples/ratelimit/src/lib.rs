//! A rate limiter: a token bucket per key, each key its own object
//! (`/{key}/`). A bucket holds up to `capacity` tokens and gains `per_second`
//! of them every second; taking more than it has is refused with `429` and
//! how long to wait. Defaults come from the app's config; `PUT` sets a key's
//! own.
//!
//! - `POST /{key}/take?n=1`: takes `n` tokens: `200`, or `429` with `retry-after`
//! - `GET /{key}/`: the bucket now, taking nothing
//! - `PUT /{key}/` `{"capacity":5,"per_second":0.5}`: this key's limits
//!
//! There is no lock, compare-and-swap or script: an object serves one request
//! at a time, so "refill, check, take" can't interleave with another take.

use std::time::{SystemTime, UNIX_EPOCH};

use nanovm_sdk::{Request, Response, Result, config, kv, serde_json};
use serde::{Deserialize, Serialize};

nanovm_sdk::serve!(handle);

#[derive(Serialize, Deserialize)]
struct Limits {
    capacity: f64,
    per_second: f64,
}

#[derive(Serialize, Deserialize)]
struct Bucket {
    tokens: f64,
    /// When `tokens` was right, in ms since the epoch.
    at: u64,
}

async fn handle(req: Request) -> Result {
    let limits = match kv::get("limits").await? {
        Some(json) => serde_json::from_slice(&json)?,
        None => Limits { capacity: setting("capacity", 10.0), per_second: setting("per_second", 1.0) },
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let mut bucket = match kv::get("bucket").await? {
        Some(json) => serde_json::from_slice(&json)?,
        None => Bucket { tokens: limits.capacity, at: now },
    };
    // Refill for the time since it was last right.
    let elapsed = now.saturating_sub(bucket.at) as f64 / 1000.0;
    bucket = Bucket { tokens: (bucket.tokens + elapsed * limits.per_second).min(limits.capacity), at: now };

    Ok(match (req.method(), req.path()) {
        ("POST", "/take") => {
            let n = req.query("n").unwrap_or("1").parse().unwrap_or(f64::NAN);
            if !(n > 0.0 && n <= limits.capacity) {
                return Ok(Response::text("n is more than 0 and at most the capacity\n").status(400));
            }
            if bucket.tokens < n {
                let wait_ms = ((n - bucket.tokens) / limits.per_second * 1000.0).ceil();
                let body = format!(r#"{{"allowed":false,"remaining":{},"retry_after_ms":{wait_ms}}}"#, bucket.tokens.floor());
                // Nothing to store: the bucket refills by the clock.
                return Ok(Response::json(body).status(429).header("retry-after", (wait_ms / 1000.0).ceil().to_string()));
            }
            bucket.tokens -= n;
            kv::set("bucket", serde_json::to_vec(&bucket)?).await?;
            Response::json(format!(r#"{{"allowed":true,"remaining":{}}}"#, bucket.tokens.floor()))
                .header("ratelimit-remaining", bucket.tokens.floor().to_string())
        }
        ("GET", "/") => Response::json(format!(
            r#"{{"tokens":{},"capacity":{},"per_second":{}}}"#,
            bucket.tokens, limits.capacity, limits.per_second
        )),
        ("PUT", "/") => {
            let Ok(new) = req.json::<Limits>() else { return Ok(Response::text("bad limits\n").status(400)) };
            if !(new.capacity >= 1.0 && new.capacity <= 1e9 && new.per_second > 0.0 && new.per_second <= 1e9) {
                return Ok(Response::text("capacity is 1 to 1e9, per_second more than 0\n").status(400));
            }
            kv::set("limits", serde_json::to_vec(&new)?).await?;
            // A new bucket starts full.
            kv::delete("bucket").await?;
            Response::new(204)
        }
        _ => Response::not_found(),
    })
}

/// A number from the app's config, or `default`.
fn setting(key: &str, default: f64) -> f64 {
    config::get(key).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(default)
}
