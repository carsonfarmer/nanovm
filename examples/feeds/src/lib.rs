//! A feed aggregator. Each feed is an object (`/f-{name}/`) that fetches
//! its RSS or Atom document over egress, with a conditional GET (`ETag`), and
//! keeps each item once, however often it is fetched; each reader is an
//! object (`/{reader}/`) whose timeline gathers new items from the feeds it
//! follows, through calls between objects. Many readers of one feed cost one
//! fetch: the feed object is the cache.
//!
//! - `PUT /f-{name}/` `{"url":"https://…"}`, `POST /f-{name}/refresh` (from
//!   an external clock): `{"new":n}`; `GET /f-{name}/items?after=n`
//! - `PUT /{reader}/follows` `["f-…", …]`; `GET /{reader}/timeline`: newest
//!   first, after pulling what is new

use nanovm_sdk::http::header::CONTENT_TYPE;
use nanovm_sdk::http::{Request, Response};
use nanovm_sdk::{Body, RequestExt, Result, fetch, kv, object, serde_json};
use serde::{Deserialize, Serialize};

nanovm_sdk::serve!(handle);

#[derive(Serialize, Deserialize, Clone)]
struct Item {
    seq: i64,
    feed: String,
    title: String,
    link: String,
}

async fn handle(req: Request<Body>) -> Result {
    if nanovm_sdk::id().starts_with("f-") {
        feed(&req).await
    } else {
        reader(&req).await
    }
}

async fn feed(req: &Request<Body>) -> Result {
    Ok(match (req.method().as_str(), req.uri().path()) {
        ("PUT", "/") => {
            let url = req.body().json::<serde_json::Value>().ok().and_then(|v| v["url"].as_str().map(String::from));
            match url.filter(|u| u.starts_with("http")) {
                Some(url) => {
                    kv::set("url", url.as_str()).await?;
                    Response::builder().status(204).body(Body::empty())?
                }
                None => text(400, "{\"url\": \"https://…\"}\n")?,
            }
        }
        ("POST", "/refresh") => {
            let Some(url) = kv::get_text("url").await? else { return text(404, "not found") };
            let mut get = fetch(url.as_str()).header("accept", "application/rss+xml, application/atom+xml");
            if let Some(etag) = kv::get_text("etag").await? {
                get = get.header("if-none-match", etag);
            }
            let reply = get.send().await?;
            if reply.status() == 304 {
                return json(200, r#"{"new":0,"unchanged":true}"#);
            }
            if reply.status() != 200 {
                return text(502, format!("the feed answered {}\n", reply.status()));
            }
            let Ok(found) = items(&String::from_utf8_lossy(reply.body())) else {
                return text(502, "not RSS or Atom\n");
            };
            let mut new = 0;
            // Oldest first, so sequence numbers follow the feed's order.
            for (guid, title, link) in found.into_iter().rev() {
                let key = format!("guid:{}", fnv(&guid));
                if kv::exists(&key).await? {
                    continue;
                }
                let seq = kv::increment("seq", 1).await?;
                kv::set(&key, seq.to_string()).await?;
                let item = Item { seq, feed: nanovm_sdk::id(), title, link };
                kv::set(&format!("item:{seq:010}"), serde_json::to_vec(&item)?).await?;
                new += 1;
            }
            if let Some((_, etag)) = reply.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("etag")) {
                kv::set("etag", etag.as_str()).await?;
            }
            json(200, format!(r#"{{"new":{new}}}"#))?
        }
        ("GET", "/items") => {
            let after: i64 = req.query_or("after", 0);
            let mut keys: Vec<String> = kv::keys().await?.into_iter().filter(|k| k.starts_with("item:")).collect();
            keys.sort();
            let mut out = vec![];
            for key in keys.into_iter().filter(|k| k[5..].parse::<i64>().is_ok_and(|s| s > after)).take(100) {
                out.push(serde_json::from_slice::<Item>(&kv::get(&key).await?.unwrap_or_default())?);
            }
            json(200, serde_json::to_string(&out)?)?
        }
        _ => text(404, "not found")?,
    })
}

async fn reader(req: &Request<Body>) -> Result {
    Ok(match (req.method().as_str(), req.uri().path()) {
        ("PUT", "/follows") => match req.body().json::<Vec<String>>() {
            Ok(feeds) if feeds.len() <= 50 && feeds.iter().all(|f| f.starts_with("f-")) => {
                kv::set("follows", serde_json::to_vec(&feeds)?).await?;
                Response::builder().status(204).body(Body::empty())?
            }
            _ => text(400, "[\"f-…\", …], up to 50\n")?,
        },
        ("GET", "/timeline") => {
            let follows: Vec<String> = serde_json::from_slice(&kv::get("follows").await?.unwrap_or(b"[]".into()))?;
            let mut timeline: Vec<Item> = serde_json::from_slice(&kv::get("timeline").await?.unwrap_or(b"[]".into()))?;
            let mut links: std::collections::HashSet<String> = timeline.iter().map(|i| i.link.clone()).collect();
            for feed in follows {
                // How far this reader has read each feed.
                let read_key = format!("read:{feed}");
                let after = kv::get_text(&read_key).await?.unwrap_or("0".into());
                let reply = object(&feed).get(&format!("/items?after={after}")).await?;
                let Ok(new) = serde_json::from_slice::<Vec<Item>>(reply.body()) else { continue };
                if let Some(last) = new.last() {
                    kv::set(&read_key, last.seq.to_string()).await?;
                }
                // The same link from two feeds is one entry.
                timeline.extend(new.into_iter().filter(|i| links.insert(i.link.clone())));
            }
            // The newest 500.
            timeline.drain(..timeline.len().saturating_sub(500));
            kv::set("timeline", serde_json::to_vec(&timeline)?).await?;
            let newest_first: Vec<&Item> = timeline.iter().rev().collect();
            json(200, serde_json::to_string(&newest_first)?)?
        }
        _ => text(404, "not found")?,
    })
}

/// The `(guid, title, link)` of each item of an RSS or Atom document,
/// newest first as feeds list them.
fn items(xml: &str) -> core::result::Result<Vec<(String, String, String)>, roxmltree::Error> {
    let doc = roxmltree::Document::parse(xml)?;
    fn child<'a>(n: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
        n.children().find(|c| c.has_tag_name(name)).and_then(|c| c.text()).map(str::trim)
    }
    let mut out = vec![];
    for n in doc.descendants().filter(|n| n.has_tag_name("item") || n.has_tag_name("entry")) {
        // RSS: <link>url</link>; Atom: <link href="url"/>.
        let link = child(n, "link").filter(|l| !l.is_empty()).map(String::from).or_else(|| {
            n.children().find(|c| c.has_tag_name("link")).and_then(|l| l.attribute("href")).map(String::from)
        });
        let link = link.unwrap_or_default();
        let guid = child(n, "guid").or(child(n, "id")).map(String::from).unwrap_or_else(|| link.clone());
        out.push((guid, child(n, "title").unwrap_or_default().to_string(), link));
    }
    Ok(out)
}

/// A key for a guid of any length: FNV-1a, hex.
fn fnv(s: &str) -> String {
    format!("{:016x}", s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3)))
}

/// A JSON answer.
fn json(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "application/json").body(body.into())?)
}

/// A plain-text answer.
fn text(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "text/plain; charset=utf-8").body(body.into())?)
}
