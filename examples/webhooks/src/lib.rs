//! A webhook receiver and relay. Each endpoint is an object (`/{hook}/`)
//! with a secret and the URLs it relays to. A webhook is checked against its
//! HMAC-SHA256 signature (GitHub's `x-hub-signature-256`), stored, answered
//! `202` at once, and then relayed to every target after the answer has gone
//! (`nanovm_sdk::after`). Targets that fail are retried with backoff when an
//! external clock sends `POST /{hook}/retry`; after five tries a delivery is
//! dead. Targets must be in the app's `egress` list.
//!
//! - `PUT /{hook}/config` `{"secret":"…","targets":["https://…"]}`
//! - `POST /{hook}/in` (signed): `202 {"id":n}`
//! - `POST /{hook}/retry`: `{"retried":n}`
//! - `GET /{hook}/deliveries`

use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use nanovm_sdk::{Request, Response, Result, fetch, kv, serde_json};
use serde::{Deserialize, Serialize};

nanovm_sdk::serve!(handle);

const TRIES: u32 = 5;

#[derive(Serialize, Deserialize)]
struct Config {
    secret: String,
    targets: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Delivery {
    id: i64,
    body: String,
    content_type: String,
    /// Targets still to reach, and those reached.
    pending: Vec<String>,
    done: Vec<String>,
    tries: u32,
    next_at: u64,
    dead: bool,
    error: Option<String>,
}

async fn handle(req: Request) -> Result {
    let now = now()?;
    Ok(match (req.method(), req.path()) {
        ("PUT", "/config") => match req.json::<Config>() {
            Ok(c) if !c.secret.is_empty() && c.targets.iter().all(|t| t.starts_with("http")) && c.targets.len() <= 10 => {
                kv::set("config", serde_json::to_vec(&c)?).await?;
                Response::new(204)
            }
            _ => Response::text("{\"secret\", \"targets\": up to 10 URLs}\n").status(400),
        },
        ("POST", "/in") => {
            let Some(config) = kv::get("config").await? else { return Ok(Response::not_found()) };
            let config: Config = serde_json::from_slice(&config)?;
            if !signed(&config.secret, req.header("x-hub-signature-256").unwrap_or_default(), req.body()) {
                return Ok(Response::text("bad signature\n").status(401));
            }
            let id = kv::increment("next", 1).await?;
            let delivery = Delivery {
                id,
                body: req.text()?.into(),
                content_type: req.header("content-type").unwrap_or("application/json").into(),
                pending: config.targets,
                done: vec![],
                tries: 0,
                next_at: now,
                dead: false,
                error: None,
            };
            save(&delivery).await?;
            // The sender has its answer before any target is tried.
            nanovm_sdk::after(async move {
                let _ = deliver(delivery).await;
            });
            Response::json(format!(r#"{{"id":{id}}}"#)).status(202)
        }
        ("POST", "/retry") => {
            let mut retried = 0;
            for d in all().await? {
                if !d.pending.is_empty() && !d.dead && d.next_at <= now {
                    deliver(d).await?;
                    retried += 1;
                }
            }
            Response::json(format!(r#"{{"retried":{retried}}}"#))
        }
        ("GET", "/deliveries") => Response::json(serde_json::to_string(&all().await?)?),
        _ => Response::not_found(),
    })
}

/// Tries every pending target once, then records the outcome and when to
/// try again: 1 s, 2 s, 4 s, 8 s after each failed round.
async fn deliver(mut d: Delivery) -> Result<()> {
    let mut failed = vec![];
    for target in std::mem::take(&mut d.pending) {
        let sent = fetch(target.as_str())
            .method("POST")
            .header("content-type", &d.content_type)
            .header("x-relay-delivery", d.id.to_string())
            .body(d.body.as_str())
            .send()
            .await;
        match sent {
            Ok(r) if (200..300).contains(&r.status) => d.done.push(target),
            Ok(r) => {
                d.error = Some(format!("{target}: {}", r.status));
                failed.push(target);
            }
            Err(e) => {
                d.error = Some(format!("{target}: {e:?}"));
                failed.push(target);
            }
        }
    }
    d.pending = failed;
    if !d.pending.is_empty() {
        d.tries += 1;
        d.dead = d.tries >= TRIES;
        d.next_at = now()? + (1000u64 << (d.tries - 1).min(10));
    }
    save(&d).await
}

/// `signature` is `sha256=` and the hex HMAC-SHA256 of `body` under `secret`;
/// compared in constant time.
fn signed(secret: &str, signature: &str, body: &[u8]) -> bool {
    let Some(hex) = signature.strip_prefix("sha256=") else { return false };
    let bytes: Option<Vec<u8>> =
        (0..hex.len()).step_by(2).map(|i| hex.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok())).collect();
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(body);
    bytes.is_some_and(|b| mac.verify_slice(&b).is_ok())
}

async fn save(d: &Delivery) -> Result<()> {
    Ok(kv::set(&format!("d:{:010}", d.id), serde_json::to_vec(d)?).await?)
}

async fn all() -> Result<Vec<Delivery>> {
    let mut keys: Vec<String> = kv::keys().await?.into_iter().filter(|k| k.starts_with("d:")).collect();
    keys.sort();
    let mut out = vec![];
    for key in keys {
        out.push(serde_json::from_slice(&kv::get(&key).await?.unwrap_or_default())?);
    }
    Ok(out)
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
}
