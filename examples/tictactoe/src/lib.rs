//! A game lobby: tic-tac-toe rooms as objects. The object `lobby` pairs
//! players: the first to ask waits in a new game, the next joins it, and the
//! lobby starts the game with a call to its object. Each game (`/g-…/`) is
//! the one authority on its board: it checks every move (whose turn, which
//! square, whether the game is over) and pushes the board to its players'
//! WebSockets.
//!
//! - `POST /lobby/match?player=ann`: `{"game":"g-…","you":"x"|"o"}`
//! - `/{game}/ws?player=ann`: a WebSocket that gets the game after every
//!   change; it sends a square, `0`-`8`, to move there
//! - `POST /{game}/move?player=ann&square=4`: the same over HTTP
//! - `GET /{game}/`: the game
//!
//! Players are names, taken on trust: a real game would sign them in.

use nanovm_sdk::http::header::CONTENT_TYPE;
use nanovm_sdk::http::{Request, Response};
use nanovm_sdk::{Body, RequestExt, Result, kv, object, serde_json, ws};
use serde::{Deserialize, Serialize};
use serde_json::json;

nanovm_sdk::serve!(handle);

#[derive(Serialize, Deserialize, Default)]
struct Game {
    x: String,
    o: Option<String>,
    /// Nine squares, row by row: `x`, `o` or `.`.
    board: String,
    /// Whose turn it is: `x` or `o`.
    turn: char,
    /// `x`, `o`, `draw`, or nothing while it goes on.
    result: Option<String>,
}

async fn handle(req: Request<Body>) -> Result {
    let player = req.query("player").unwrap_or_default().to_string();
    if nanovm_sdk::id() == "lobby" {
        return match (req.method().as_str(), req.uri().path()) {
            ("POST", "/match") if (1..=32).contains(&player.len()) => matchmake(&player).await,
            _ => text(404, "not found"),
        };
    }
    let game: Option<Game> = match kv::get("game").await? {
        Some(json) => Some(serde_json::from_slice(&json)?),
        None => None,
    };
    if let Some(events) = ws::events(&req) {
        let Some(game) = game else { return text(404, "not found") };
        return socket(&player, events, game).await;
    }
    Ok(match (req.method().as_str(), req.uri().path(), game) {
        ("GET", "/", Some(game)) => json(200, serde_json::to_string(&game)?)?,
        ("POST", "/move", Some(game)) => match play(&player, &req.query("square").unwrap_or_default(), game).await? {
            Ok(game) => json(200, game)?,
            Err(why) => json(409, json!({ "error": why }).to_string())?,
        },
        // For the lobby. Anyone can send these (nanovm doesn't say who is
        // calling, W-602), so they only ever make a game once and fill its
        // second seat once; the lobby copes with a seat taken behind its back.
        ("POST", "/new", None) => {
            let x = req.body().text()?.to_string();
            let game = Game { x, board: ".........".into(), turn: 'x', ..Default::default() };
            kv::set("game", serde_json::to_vec(&game)?).await?;
            Response::builder().status(201).body(Body::empty())?
        }
        ("POST", "/join", Some(mut game)) if game.o.is_none() && req.body().text()? != game.x => {
            game.o = Some(req.body().text()?.to_string());
            let state = serde_json::to_string(&game)?;
            kv::set("game", state.as_str()).await?;
            ws::publish("game", &state).await?;
            json(200, state)?
        }
        _ => text(404, "not found")?,
    })
}

/// Pairs `player` with whoever is waiting, or makes them wait in a new game.
/// The lobby serves one request at a time, so two players can't both join
/// the same waiting game.
async fn matchmake(player: &str) -> Result {
    if let Some(waiting) = kv::get("waiting").await? {
        let (game, x): (String, String) = serde_json::from_slice(&waiting)?;
        if x == player {
            return json(200, json!({ "game": game, "you": "x" }).to_string());
        }
        let reply = object(&game).post("/join", player).await?;
        kv::delete("waiting").await?;
        if reply.status() == 200 {
            return json(200, json!({ "game": game, "you": "o" }).to_string());
        }
    }
    let game = format!("g-{:08x}", nanovm_sdk::wasip3::random::random::get_random_u64() as u32);
    let reply = object(&game).post("/new", player).await?;
    if reply.status() != 201 {
        return Err(nanovm_sdk::Error::new(format!("{game} would not start: {}", reply.status())));
    }
    kv::set("waiting", serde_json::to_vec(&(&game, player))?).await?;
    json(200, json!({ "game": game, "you": "x" }).to_string())
}

async fn socket(player: &str, events: Vec<ws::Event>, game: Game) -> Result {
    let mut answer = ws::Answer::default();
    for event in events {
        match event {
            ws::Event::Open => {
                answer.accept().subscribe("game").text(&serde_json::to_string(&game)?);
            }
            ws::Event::Text(square) => {
                // Read afresh: an earlier move in this same batch may have changed it.
                let game = serde_json::from_slice(&kv::get("game").await?.unwrap_or_default())?;
                if let Err(why) = play(player, &square, game).await? {
                    answer.text(&json!({ "error": why }).to_string());
                }
            }
            _ => {}
        }
    }
    Ok(answer.into())
}

/// Plays `player`'s move to `square`, if it is a legal one: the game as JSON
/// (also published to its players), or why not.
async fn play(player: &str, square: &str, mut game: Game) -> Result<core::result::Result<String, &'static str>> {
    let Some(o) = &game.o else { return Ok(Err("waiting for an opponent")) };
    let mover = if game.turn == 'x' { &game.x } else { o };
    let square = square.trim().parse::<usize>().ok().filter(|s| *s < 9);
    let why = match square {
        _ if game.result.is_some() => "the game is over",
        _ if player != mover => "not your turn",
        None => "a square is 0 to 8",
        Some(s) if game.board.as_bytes()[s] != b'.' => "that square is taken",
        Some(s) => {
            game.board.replace_range(s..=s, &game.turn.to_string());
            game.result = outcome(game.board.as_bytes());
            game.turn = if game.turn == 'x' { 'o' } else { 'x' };
            let json = serde_json::to_string(&game)?;
            kv::set("game", json.as_str()).await?;
            ws::publish("game", &json).await?;
            return Ok(Ok(json));
        }
    };
    Ok(Err(why))
}

fn outcome(b: &[u8]) -> Option<String> {
    const LINES: [[usize; 3]; 8] = [[0, 1, 2], [3, 4, 5], [6, 7, 8], [0, 3, 6], [1, 4, 7], [2, 5, 8], [0, 4, 8], [2, 4, 6]];
    for [i, j, k] in LINES {
        if b[i] != b'.' && b[i] == b[j] && b[j] == b[k] {
            return Some((b[i] as char).to_string());
        }
    }
    (!b.contains(&b'.')).then(|| "draw".into())
}

/// A JSON answer.
fn json(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "application/json").body(body.into())?)
}

/// A plain-text answer.
fn text(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "text/plain; charset=utf-8").body(body.into())?)
}
