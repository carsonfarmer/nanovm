//! A URL shortener with click analytics. Every short link is its own object,
//! `/{code}`: the link's target and its clicks, rolled up per day, live in
//! that object's key-value store, so links share nothing and a busy link
//! never slows another. The object `new` mints codes.
//!
//! - `POST /new/` with a URL as the body: a link with a random code, `201`
//! - `POST /{code}/` with a URL as the body: a link with that code, `201`, or
//!   `409` if the code is taken
//! - `GET /{code}`: `302` to the target, counting a click for today (UTC)
//! - `GET /{code}/stats`: the target, total clicks and clicks per day

use std::time::{SystemTime, UNIX_EPOCH};

use nanovm_sdk::{Request, Response, Result, kv, object, quote};

nanovm_sdk::serve!(handle);

const NEW: &str = "new";

async fn handle(req: Request) -> Result {
    if nanovm_sdk::id() == NEW {
        return match req.method() {
            "POST" => mint(req.text()?).await,
            _ => Ok(Response::not_found()),
        };
    }
    let Some(url) = kv::get_text("url").await? else {
        return Ok(match req.method() {
            "POST" if req.path() == "/" => create(req.text()?).await?,
            _ => Response::not_found(),
        });
    };
    Ok(match (req.method(), req.path()) {
        ("GET", "/") => {
            kv::increment("clicks", 1).await?;
            kv::increment(&format!("day:{}", today()?), 1).await?;
            Response::new(302).header("location", &url)
        }
        ("HEAD", "/") => Response::new(302).header("location", &url),
        ("GET", "/stats") => Response::json(stats(&url).await?),
        ("POST", "/") => Response::text("that code is taken\n").status(409),
        _ => Response::not_found(),
    })
}

/// Stores the link's target. The object serves one request at a time, so
/// two requests for the same code can't both see it free.
async fn create(url: &str) -> Result {
    let url = url.trim();
    let ok = (url.starts_with("https://") || url.starts_with("http://")) && url.len() <= 2048;
    if !ok || url.contains(char::is_whitespace) {
        return Ok(Response::text("the body is an http(s) URL of at most 2048 bytes\n").status(400));
    }
    kv::set("url", url).await?;
    let code = nanovm_sdk::id();
    Ok(Response::json(format!(r#"{{"code":{},"url":{}}}"#, quote(&code), quote(url))).status(201))
}

/// Picks a random code and asks that object to take the link; a code that
/// is taken (or a bad URL) is the answer of the object that holds it.
async fn mint(url: &str) -> Result {
    for _ in 0..5 {
        let code = random_code(7);
        let reply = object(&code).post("/", url).send().await?;
        if reply.status != 409 {
            return Ok(Response::bytes("application/json", reply.body).status(reply.status));
        }
    }
    Ok(Response::text("no free code; try again\n").status(503))
}

async fn stats(url: &str) -> Result<String> {
    let mut days = vec![];
    for key in kv::keys().await? {
        if let Some(day) = key.strip_prefix("day:") {
            days.push(format!("{}:{}", quote(day), kv::get_text(&key).await?.unwrap_or_default()));
        }
    }
    days.sort();
    let clicks = kv::get_text("clicks").await?.unwrap_or("0".into());
    Ok(format!(r#"{{"url":{},"clicks":{clicks},"days":{{{}}}}}"#, quote(url), days.join(",")))
}

/// `n` random letters and digits.
fn random_code(n: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let bytes = nanovm_sdk::wasip3::random::random::get_random_bytes(n as u64);
    bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect()
}

/// Today's date in UTC, `YYYY-MM-DD` (Howard Hinnant's `civil_from_days`).
fn today() -> Result<String> {
    let days = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64 / 86_400 + 719_468;
    let (era, doe) = (days.div_euclid(146_097), days.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    Ok(format!("{:04}-{m:02}-{d:02}", yoe + era * 400 + i64::from(m <= 2)))
}
