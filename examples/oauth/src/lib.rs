//! Sign-in with GitHub (OAuth 2.0) and sessions. The object `auth` runs the
//! flow: it sends the browser to GitHub with a one-time `state`, and on the
//! way back checks it, trades the code for a token and the token for the
//! user's login over egress, and starts a session. Each user's object
//! (`/u-{login}/`) accepts a session cookie only once `auth` has vouched for
//! it, then remembers that it did.
//!
//! Why `auth` keeps the sessions: a call between objects can't prove who
//! sent it (W-602), so a user object that took new sessions from a call
//! would take them from anyone. Instead it asks `auth` about a cookie it was
//! shown: knowing a session's id is what proves the session.
//!
//! - `GET /auth/login`: `302` to GitHub
//! - `GET /auth/callback?code=…&state=…`: `302` to `/u-{login}/me`, with the
//!   `session` cookie
//! - `GET /u-{login}/me`: the profile, or `401`; `POST /u-{login}/logout`

use std::time::{SystemTime, UNIX_EPOCH};

use nanovm_sdk::{Request, Response, Result, config, fetch, kv, object, serde_json};
use serde_json::{Value, json};

nanovm_sdk::serve!(handle);

const STATE_MS: u64 = 10 * 60 * 1000;
const SESSION_MS: u64 = 30 * 24 * 3600 * 1000;

async fn handle(req: Request) -> Result {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    match nanovm_sdk::id().as_str() {
        "auth" => auth(&req, now).await,
        id if id.starts_with("u-") => user(&req, now).await,
        _ => Ok(Response::not_found()),
    }
}

async fn auth(req: &Request, now: u64) -> Result {
    let setting = |k: &str| config::get(k).ok().flatten().unwrap_or_default();
    Ok(match req.path() {
        "/login" => {
            let state = random_hex(16);
            kv::set(&format!("state:{state}"), (now + STATE_MS).to_string()).await?;
            let to = format!(
                "{}?client_id={}&redirect_uri={}&state={state}&scope=read:user",
                setting("authorize_url"),
                setting("client_id"),
                setting("redirect_uri")
            );
            Response::new(302).header("location", to)
        }
        "/callback" => {
            // The state proves this browser started the flow here, once.
            let key = format!("state:{}", req.query("state").unwrap_or_default());
            let fresh = kv::get_text(&key).await?.and_then(|t| t.parse::<u64>().ok()).is_some_and(|t| t > now);
            kv::delete(&key).await?;
            if !fresh {
                return Ok(Response::text("this sign-in link is used, expired or not ours\n").status(400));
            }
            let token = fetch(setting("token_url"))
                .method("POST")
                .header("accept", "application/json")
                .header("content-type", "application/json")
                .body(
                    json!({
                        "client_id": setting("client_id"),
                        "client_secret": nanovm_sdk::env("CLIENT_SECRET"),
                        "code": req.query("code").unwrap_or_default(),
                    })
                    .to_string(),
                )
                .send()
                .await?;
            let token: Value = serde_json::from_slice(&token.body).unwrap_or_default();
            let Some(token) = token["access_token"].as_str() else {
                return Ok(Response::text("GitHub refused the code\n").status(401));
            };
            let me = fetch(setting("user_url")).header("authorization", format!("Bearer {token}")).header("user-agent", "nanovm").send().await?;
            let me: Value = serde_json::from_slice(&me.body).unwrap_or_default();
            let login = me["login"].as_str().filter(|l| (1..=39).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
            let Some(login) = login else { return Ok(Response::text("no GitHub user\n").status(502)) };
            let session = random_hex(32);
            let profile = json!({ "login": login, "id": me["id"], "name": me["name"] });
            kv::set(&format!("session:{session}"), json!({ "profile": profile, "until": now + SESSION_MS }).to_string()).await?;
            let cookie = format!("session={login}.{session}; Path=/; Max-Age={}; HttpOnly; SameSite=Lax", SESSION_MS / 1000);
            Response::new(302).header("location", format!("/u-{login}/me")).header("set-cookie", cookie)
        }
        // For user objects, about a cookie they were shown.
        "/verify" => match session(req.query("session")).await? {
            Some(s) if s["until"].as_u64().is_some_and(|t| t > now) => Response::json(s.to_string()),
            _ => Response::not_found(),
        },
        "/logout" if req.method() == "POST" => {
            if let Some(id) = req.query("session").filter(|s| s.len() == 64) {
                kv::delete(&format!("session:{id}")).await?;
            }
            Response::new(204)
        }
        _ => Response::not_found(),
    })
}

async fn user(req: &Request, now: u64) -> Result {
    // A cookie is `{login}.{session}`: only this user's own sessions count.
    let login = &nanovm_sdk::id()[2..];
    let cookie = req.cookie("session").and_then(|c| c.split_once('.')).filter(|(l, s)| *l == login && s.len() == 64);
    let session = cookie.map(|(_, s)| s.to_string());
    let signed_in = match &session {
        Some(id) => match kv::get_text(&format!("session:{id}")).await?.and_then(|t| t.parse::<u64>().ok()) {
            Some(until) => until > now,
            // Not seen here before: ask `auth`, and remember its answer.
            None => {
                let reply = object("auth").get(&format!("/verify?session={id}")).send().await?;
                let vouched: Value = serde_json::from_slice(&reply.body).unwrap_or_default();
                let ok = reply.status == 200 && vouched["profile"]["login"] == login;
                if ok {
                    kv::set(&format!("session:{id}"), vouched["until"].to_string()).await?;
                    kv::set("profile", vouched["profile"].to_string()).await?;
                }
                ok
            }
        },
        None => false,
    };
    Ok(match (req.method(), req.path()) {
        ("GET", "/me") if signed_in => Response::json(kv::get_text("profile").await?.unwrap_or_default()),
        ("POST", "/logout") if signed_in => {
            let id = session.unwrap_or_default();
            kv::delete(&format!("session:{id}")).await?;
            object("auth").post(&format!("/logout?session={id}"), "").send().await?;
            Response::new(204).header("set-cookie", "session=; Path=/; Max-Age=0")
        }
        ("GET", "/me") | ("POST", "/logout") => Response::text("sign in at /auth/login\n").status(401),
        _ => Response::not_found(),
    })
}

async fn session(id: Option<&str>) -> Result<Option<Value>> {
    let Some(id) = id.filter(|s| s.len() == 64) else { return Ok(None) };
    Ok(kv::get_text(&format!("session:{id}")).await?.and_then(|s| serde_json::from_str(&s).ok()))
}

fn random_hex(bytes: u64) -> String {
    nanovm_sdk::wasip3::random::random::get_random_bytes(bytes).iter().map(|b| format!("{b:02x}")).collect()
}
