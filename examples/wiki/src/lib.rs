//! A wiki with history. Each wiki is an object (`/{wiki}/`) with its own
//! SQLite database: every save of a page is a new revision, never an
//! overwrite. Pages are Markdown, rendered to HTML on the way out (with any
//! raw HTML in them escaped), and any two revisions can be diffed. A save
//! names the revision it was edited from; if someone saved in between, it
//! is refused with `409` rather than silently overwriting their edit.
//!
//! A page is anyone's input, so nothing in it may run in a reader's
//! browser: its raw HTML is escaped, its links and images keep only http,
//! https, mailto and relative destinations (a `javascript:` link becomes
//! its text), and the page's Content-Security-Policy refuses script anyway.
//!
//! - `PUT /{wiki}/pages/{name}?author=…` (body: Markdown; `if-match: <rev>`,
//!   `0` for a new page): `201 {"rev":n}`, or `409` with the latest revision
//! - `GET /{wiki}/pages/{name}` (`?rev=n`, `?raw=1`): the page as HTML (or
//!   Markdown), its revision in `etag`
//! - `GET /{wiki}/pages/{name}/history`, `GET /{wiki}/pages/{name}/diff?from=1&to=2`
//! - `GET /{wiki}/`: every page and its latest revision

use std::cell::Cell;
use std::time::{SystemTime, UNIX_EPOCH};

use nanovm_sdk::{Request, Response, Result, quote, sqlite};
use pulldown_cmark::{CowStr, Event, Parser, Tag, TagEnd, html};

nanovm_sdk::serve!(handle);

async fn handle(req: Request) -> Result {
    schema().await?;
    let path: Vec<&str> = req.path().trim_matches('/').split('/').collect();
    Ok(match (req.method(), path.as_slice()) {
        ("GET", [""]) => {
            let pages = sqlite::query("SELECT page, max(rev) AS rev FROM revisions GROUP BY page ORDER BY page", &[]).await?;
            Response::json(pages.json().to_string())
        }
        ("PUT", ["pages", page]) if valid(page) => save(page, &req).await?,
        ("GET", ["pages", page]) => {
            let rev = match req.query("rev") {
                Some(rev) => rev.parse().unwrap_or(-1),
                None => latest(page).await?,
            };
            let found = sqlite::query("SELECT body FROM revisions WHERE page = ? AND rev = ?", &[page, &rev]).await?;
            let Some(row) = found.first() else { return Ok(Response::not_found()) };
            let body: String = row.get("body")?;
            let page = match req.query("raw") {
                Some(_) => Response::bytes("text/markdown; charset=utf-8", body),
                None => Response::html(render(&body))
                    .header("content-security-policy", CSP)
                    .header("x-content-type-options", "nosniff"),
            };
            page.header("etag", format!("\"{rev}\""))
        }
        ("GET", ["pages", page, "history"]) => {
            let history = sqlite::query(
                "SELECT rev, author, at, length(body) AS bytes FROM revisions WHERE page = ? ORDER BY rev DESC LIMIT 100",
                &[page],
            )
            .await?;
            if history.is_empty() { Response::not_found() } else { Response::json(history.json().to_string()) }
        }
        ("GET", ["pages", page, "diff"]) => {
            let body = async |rev: &str| -> Result<Option<String>> {
                let rows = sqlite::query("SELECT body FROM revisions WHERE page = ? AND rev = ?", &[page, &rev]).await?;
                Ok(match rows.first() {
                    Some(row) => Some(row.get("body")?),
                    None => None,
                })
            };
            let (from, to) = (req.query("from").unwrap_or("0"), req.query("to").unwrap_or("0"));
            match (body(from).await?, body(to).await?) {
                (Some(a), Some(b)) => {
                    let diff = similar::TextDiff::from_lines(&a, &b);
                    let diff = diff.unified_diff().header(&format!("{page}@{from}"), &format!("{page}@{to}")).to_string();
                    Response::bytes("text/x-diff; charset=utf-8", diff)
                }
                _ => Response::not_found(),
            }
        }
        _ => Response::not_found(),
    })
}

/// A new revision of `page`, if it was edited from the latest one. Checking
/// and saving are one statement, and the wiki serves one request at a time.
async fn save(page: &str, req: &Request) -> Result {
    let author = req.query("author").unwrap_or("anonymous");
    let base: i64 = match req.header("if-match").map(|r| r.trim_matches('"').parse()) {
        Some(Ok(rev)) => rev,
        _ => return Ok(Response::text("if-match: the revision you edited (0 for a new page)\n").status(428)),
    };
    let at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;
    let saved = sqlite::execute(
        "INSERT INTO revisions SELECT ?1, coalesce(max(rev), 0) + 1, ?2, ?3, ?4 FROM revisions WHERE page = ?1 \
         HAVING coalesce(max(rev), 0) = ?5",
        &[&page, &req.text()?, &author, &at, &base],
    )
    .await?;
    let rev = latest(page).await?;
    if saved == 0 {
        let body = format!(r#"{{"error":"edited since","rev":{rev}}}"#);
        return Ok(Response::json(body).status(409).header("etag", format!("\"{rev}\"")));
    }
    Ok(Response::json(format!(r#"{{"page":{},"rev":{rev}}}"#, quote(page))).status(201).header("etag", format!("\"{rev}\"")))
}

async fn latest(page: &str) -> Result<i64> {
    let rows = sqlite::query("SELECT coalesce(max(rev), 0) AS rev FROM revisions WHERE page = ?", &[&page]).await?;
    Ok(rows.first().map(|r| r.get("rev")).transpose()?.unwrap_or(0))
}

/// A rendered page loads images and nothing else: no script, styles,
/// frames, forms or `<base>`, whatever got into it.
const CSP: &str = "default-src 'none'; img-src 'self' http: https:; base-uri 'none'; form-action 'none'";

/// Markdown to HTML, with raw HTML in the page shown as text, not run, and
/// links and images to anything but [`safe`] destinations shown as their
/// text.
fn render(markdown: &str) -> String {
    // Whether each open link or image was dropped, to drop its end too.
    let mut open = Vec::new();
    let events = Parser::new(markdown).filter_map(|e| match e {
        Event::Html(h) | Event::InlineHtml(h) => Some(Event::Text(h)),
        Event::Start(Tag::Link { ref dest_url, .. } | Tag::Image { ref dest_url, .. }) => {
            let keep = safe(dest_url);
            open.push(keep);
            keep.then_some(e)
        }
        Event::End(TagEnd::Link | TagEnd::Image) => open.pop().unwrap_or(true).then_some(e),
        e => Some(e),
    });
    let mut out = String::from("<!doctype html><meta charset=\"utf-8\">\n");
    html::push_html(&mut out, events);
    out
}

/// Whether a link or image may go to `url`: one with no scheme (relative),
/// or http, https or mailto. Read as a browser reads it: controls and
/// spaces around it and tabs and newlines in it ignored (`java\tscript:` is
/// `javascript:`), the scheme in any case.
fn safe(url: &CowStr) -> bool {
    let url: String = url.trim_matches(|c: char| c <= ' ').chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
    let Some(end) = url.find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))) else { return true };
    let scheme = &url[..end];
    if !url[end..].starts_with(':') || !scheme.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return true;
    }
    ["http", "https", "mailto"].iter().any(|s| scheme.eq_ignore_ascii_case(s))
}

/// A page name: 1-100 letters, digits, `_` or `-`.
fn valid(name: &str) -> bool {
    (1..=100).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

thread_local! {
    static READY: Cell<bool> = const { Cell::new(false) };
}

async fn schema() -> Result<()> {
    if !READY.get() {
        sqlite::execute(
            "CREATE TABLE IF NOT EXISTS revisions (page TEXT NOT NULL, rev INTEGER NOT NULL, body TEXT NOT NULL, \
             author TEXT NOT NULL, at INTEGER NOT NULL, PRIMARY KEY (page, rev))",
            &[],
        )
        .await?;
        READY.set(true);
    }
    Ok(())
}
