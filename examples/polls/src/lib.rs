//! Polls with live results. Each poll is an object (`/{poll}/`): its
//! question, each voter's choice and the tally are in its key-value store,
//! and the results go out to every WebSocket watching it as votes come in.
//! A voter is a browser, known by a `voter` cookie; voting again changes
//! the vote rather than adding one.
//!
//! - `PUT /{poll}/` `{"question":"…","options":["…","…"]}`: makes the poll, once
//! - `POST /{poll}/vote?option=N`: votes for option `N` (from 0); the results
//! - `GET /{poll}/`: the results
//! - `/{poll}/ws`: a WebSocket that gets the results, then each change

use nanovm_sdk::http::header::{CONTENT_TYPE, SET_COOKIE};
use nanovm_sdk::http::{Request, Response};
use nanovm_sdk::{Body, RequestExt, Result, kv, serde_json, ws};
use serde::{Deserialize, Serialize};

nanovm_sdk::serve!(handle);

#[derive(Serialize, Deserialize)]
struct Poll {
    question: String,
    options: Vec<String>,
}

#[derive(Serialize)]
struct Results<'a> {
    question: &'a str,
    options: Vec<Choice<'a>>,
    voters: u64,
    /// The asker's own vote, if they voted.
    #[serde(skip_serializing_if = "Option::is_none")]
    you: Option<usize>,
}

#[derive(Serialize)]
struct Choice<'a> {
    text: &'a str,
    votes: u64,
}

async fn handle(req: Request<Body>) -> Result {
    let Some(poll) = kv::get("poll").await? else {
        return match req.method().as_str() {
            "PUT" if req.uri().path() == "/" => create(&req).await,
            _ => text(404, "not found"),
        };
    };
    let poll: Poll = serde_json::from_slice(&poll)?;
    let mut tally: Vec<u64> = match kv::get("tally").await? {
        Some(json) => serde_json::from_slice(&json)?,
        None => vec![0; poll.options.len()],
    };
    // A voter id is ours only if it looks like one we gave out.
    let cookie = req.cookie("voter").filter(|v| v.len() == 16 && v.bytes().all(|b| b.is_ascii_hexdigit()));
    if let Some(events) = ws::events(&req) {
        let mut answer = ws::Answer::default();
        if events.contains(&ws::Event::Open) {
            answer.accept().subscribe("results").text(&results(&poll, &tally, None)?);
        }
        return Ok(answer.into());
    }
    Ok(match (req.method().as_str(), req.uri().path()) {
        ("GET", "/") => {
            let you = match cookie {
                Some(voter) => kv::get_text(&format!("vote:{voter}")).await?.and_then(|v| v.parse().ok()),
                None => None,
            };
            json(200, results(&poll, &tally, you)?)?
        }
        ("POST", "/vote") => {
            let Some(option) = req.query("option").and_then(|o| o.parse::<usize>().ok()).filter(|o| *o < tally.len()) else {
                return text(400, "option is the number of one of the poll's options\n");
            };
            let voter = match cookie {
                Some(voter) => voter.to_string(),
                None => nanovm_sdk::wasip3::random::random::get_random_bytes(8).iter().map(|b| format!("{b:02x}")).collect(),
            };
            let key = format!("vote:{voter}");
            let before = kv::get_text(&key).await?.and_then(|v| v.parse::<usize>().ok());
            if before != Some(option) {
                if let Some(old) = before {
                    tally[old] -= 1;
                }
                tally[option] += 1;
                kv::set(&key, option.to_string()).await?;
                kv::set("tally", serde_json::to_vec(&tally)?).await?;
                // Watchers hear of it once this vote has committed.
                ws::publish("results", &results(&poll, &tally, None)?).await?;
            }
            let cookie = format!("voter={voter}; Path=/; Max-Age=31536000; HttpOnly; SameSite=Lax");
            let mut res = json(200, results(&poll, &tally, Some(option))?)?;
            res.headers_mut().insert(SET_COOKIE, cookie.try_into()?);
            res
        }
        ("PUT", "/") => text(409, "this poll exists\n")?,
        _ => text(404, "not found")?,
    })
}

async fn create(req: &Request<Body>) -> Result {
    let poll = req.body().json::<Poll>().ok().filter(|p| {
        let text = |s: &String, max| (1..=max).contains(&s.trim().chars().count());
        text(&p.question, 200) && (2..=10).contains(&p.options.len()) && p.options.iter().all(|o| text(o, 100))
    });
    let Some(poll) = poll else {
        return text(400, "a question of 1-200 characters and 2-10 options of 1-100\n");
    };
    kv::set("poll", serde_json::to_vec(&poll)?).await?;
    json(201, results(&poll, &vec![0; poll.options.len()], None)?)
}

/// A JSON answer.
fn json(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "application/json").body(body.into())?)
}

/// A plain-text answer.
fn text(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "text/plain; charset=utf-8").body(body.into())?)
}

fn results(poll: &Poll, tally: &[u64], you: Option<usize>) -> Result<String> {
    let options = poll.options.iter().zip(tally).map(|(text, &votes)| Choice { text, votes }).collect();
    let voters = tally.iter().sum();
    Ok(serde_json::to_string(&Results { question: &poll.question, options, voters, you })?)
}
