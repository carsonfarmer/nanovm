//! Sign-in with GitHub (OAuth 2.0) and sessions. The object `auth` runs the
//! flow: it sends the browser to GitHub with a one-time `state` bound to a
//! nonce in a cookie of that browser, and on the way back checks both,
//! trades the code for a token and the token for the user's login over
//! egress, and starts a session. Each user's object
//! (`/u-{login}/`) accepts a session cookie only once `auth` has vouched for
//! it, then remembers that it did.
//!
//! Why `auth` keeps the sessions: a call between objects can't prove who
//! sent it (W-602), so a user object that took new sessions from a call
//! would take them from anyone. Instead it asks `auth` about a cookie it was
//! shown: knowing a session's id is what proves the session.
//!
//! - `GET /auth/login`: `302` to GitHub, with the nonce cookie
//! - `GET /auth/callback?code=…&state=…` (and the nonce cookie): `302` to
//!   `/u-{login}/me`, with the `session` cookie
//! - `GET /u-{login}/me`: the profile, or `401`; `POST /u-{login}/logout`

use std::time::{SystemTime, UNIX_EPOCH};

use nanovm_sdk::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, LOCATION, SET_COOKIE, USER_AGENT};
use nanovm_sdk::http::{Request, Response};
use nanovm_sdk::{Body, RequestExt, Result, config, fetch, kv, object, serde_json};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

nanovm_sdk::serve!(handle);

const STATE_MS: u64 = 10 * 60 * 1000;
const SESSION_MS: u64 = 30 * 24 * 3600 * 1000;

async fn handle(req: Request<Body>) -> Result {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    match nanovm_sdk::id().as_str() {
        "auth" => auth(&req, now).await,
        id if id.starts_with("u-") => user(&req, now).await,
        _ => text(404, "not found"),
    }
}

async fn auth(req: &Request<Body>, now: u64) -> Result {
    let setting = |k: &str| config::get(k).ok().flatten().unwrap_or_default();
    let secure = secure();
    let nonce_cookie = &named("oauth_nonce", secure);
    Ok(match req.uri().path() {
        "/login" => {
            // The browser's nonce (a new one, or the one it has, so that two
            // sign-ins started in it both work), bound to the state by its
            // hash: the state is good only in this browser.
            let nonce = req.cookie(nonce_cookie).filter(|n| n.len() == 64 && n.bytes().all(|b| b.is_ascii_hexdigit()));
            let nonce = nonce.map_or_else(|| random_hex(32), String::from);
            let state = random_hex(16);
            let bound = json!({ "until": now + STATE_MS, "browser": sha256_hex(&nonce) });
            kv::set(&format!("state:{state}"), bound.to_string()).await?;
            let to = format!(
                "{}?client_id={}&redirect_uri={}&state={state}&scope=read:user",
                setting("authorize_url"),
                setting("client_id"),
                setting("redirect_uri")
            );
            let nonce = cookie(nonce_cookie, &nonce, STATE_MS / 1000, secure);
            Response::builder().status(302).header(LOCATION, to).header(SET_COOKIE, nonce).body(Body::empty())?
        }
        "/callback" => {
            // The state proves the flow started here, once; the nonce whose
            // hash it holds, that it started in this browser. Without the
            // nonce, anyone could start a sign-in and have someone else's
            // browser finish it, signing them in as the starter.
            let key = format!("state:{}", req.query("state").unwrap_or_default());
            let bound: Value = kv::get_text(&key).await?.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
            kv::delete(&key).await?;
            let browser = req.cookie(nonce_cookie).map(sha256_hex);
            let ok = bound["until"].as_u64().is_some_and(|t| t > now) && browser.is_some_and(|b| bound["browser"] == b);
            if !ok {
                return text(400, "this sign-in link is used, expired, or not from this browser\n");
            }
            let token = Request::post(setting("token_url"))
                .header(ACCEPT, "application/json")
                .header(CONTENT_TYPE, "application/json")
                .body(
                    json!({
                        "client_id": setting("client_id"),
                        "client_secret": nanovm_sdk::env("CLIENT_SECRET"),
                        "code": req.query("code").unwrap_or_default(),
                    })
                    .to_string(),
                )?;
            let token = fetch(token).await?;
            let token: Value = serde_json::from_slice(token.body()).unwrap_or_default();
            let Some(token) = token["access_token"].as_str() else {
                return text(401, "GitHub refused the code\n");
            };
            let me = Request::get(setting("user_url")).header(AUTHORIZATION, format!("Bearer {token}")).header(USER_AGENT, "nanovm");
            let me = fetch(me.body(())?).await?;
            let me: Value = serde_json::from_slice(me.body()).unwrap_or_default();
            let login = me["login"].as_str().filter(|l| (1..=39).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
            let Some(login) = login else { return text(502, "no GitHub user\n") };
            let session = random_hex(32);
            let profile = json!({ "login": login, "id": me["id"], "name": me["name"] });
            kv::set(&format!("session:{session}"), json!({ "profile": profile, "until": now + SESSION_MS }).to_string()).await?;
            let cookie = cookie(&named("session", secure), &format!("{login}.{session}"), SESSION_MS / 1000, secure);
            Response::builder().status(302).header(LOCATION, format!("/u-{login}/me")).header(SET_COOKIE, cookie).body(Body::empty())?
        }
        // For user objects, about a cookie they were shown.
        "/verify" => match session(req.query("session").as_deref()).await? {
            Some(s) if s["until"].as_u64().is_some_and(|t| t > now) => json(200, s.to_string())?,
            _ => text(404, "not found")?,
        },
        "/logout" if req.method() == "POST" => {
            if let Some(id) = req.query("session").filter(|s| s.len() == 64) {
                kv::delete(&format!("session:{id}")).await?;
            }
            Response::builder().status(204).body(Body::empty())?
        }
        _ => text(404, "not found")?,
    })
}

async fn user(req: &Request<Body>, now: u64) -> Result {
    // A cookie is `{login}.{session}`: only this user's own sessions count.
    let login = &nanovm_sdk::id()[2..];
    let secure = secure();
    let name = named("session", secure);
    let shown = req.cookie(&name).and_then(|c| c.split_once('.')).filter(|(l, s)| *l == login && s.len() == 64);
    let session = shown.map(|(_, s)| s.to_string());
    let signed_in = match &session {
        Some(id) => match kv::get_text(&format!("session:{id}")).await?.and_then(|t| t.parse::<u64>().ok()) {
            Some(until) => until > now,
            // Not seen here before: ask `auth`, and remember its answer.
            None => {
                let reply = object("auth").get(&format!("/verify?session={id}")).await?;
                let vouched: Value = serde_json::from_slice(reply.body()).unwrap_or_default();
                let ok = reply.status() == 200 && vouched["profile"]["login"] == login;
                if ok {
                    kv::set(&format!("session:{id}"), vouched["until"].to_string()).await?;
                    kv::set("profile", vouched["profile"].to_string()).await?;
                }
                ok
            }
        },
        None => false,
    };
    Ok(match (req.method().as_str(), req.uri().path()) {
        ("GET", "/me") if signed_in => json(200, kv::get_text("profile").await?.unwrap_or_default())?,
        ("POST", "/logout") if signed_in => {
            let id = session.unwrap_or_default();
            kv::delete(&format!("session:{id}")).await?;
            object("auth").post(&format!("/logout?session={id}"), "").await?;
            Response::builder().status(204).header(SET_COOKIE, cookie(&name, "", 0, secure)).body(Body::empty())?
        }
        ("GET", "/me") | ("POST", "/logout") => text(401, "sign in at /auth/login\n")?,
        _ => text(404, "not found")?,
    })
}

async fn session(id: Option<&str>) -> Result<Option<Value>> {
    let Some(id) = id.filter(|s| s.len() == 64) else { return Ok(None) };
    Ok(kv::get_text(&format!("session:{id}")).await?.and_then(|s| serde_json::from_str(&s).ok()))
}

/// Served over https (as the callback URL says): cookies are `Secure`, and
/// `__Host-` ones, which only this origin can set. A sibling host could
/// otherwise plant its own (`Domain=` the parent), such as a session of its
/// own, and sign a browser in as someone else (login CSRF).
fn secure() -> bool {
    config::get("redirect_uri").ok().flatten().unwrap_or_default().starts_with("https://")
}

fn named(name: &str, secure: bool) -> String {
    if secure { format!("__Host-{name}") } else { name.into() }
}

/// A cookie for the whole site, out of scripts' reach, and sent on GitHub's
/// redirect back (a top-level `GET`) but not on other sites' requests.
fn cookie(name: &str, value: &str, max_age: u64, secure: bool) -> String {
    let secure = if secure { " Secure;" } else { "" };
    format!("{name}={value}; Path=/; Max-Age={max_age}; HttpOnly;{secure} SameSite=Lax")
}

fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn random_hex(bytes: u64) -> String {
    nanovm_sdk::wasip3::random::random::get_random_bytes(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// A JSON answer.
fn json(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "application/json").body(body.into())?)
}

/// A plain-text answer.
fn text(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "text/plain; charset=utf-8").body(body.into())?)
}
