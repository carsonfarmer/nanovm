//! Feature flags per tenant. The flags and their defaults are the app's
//! config (`flag.<name>` in the manifest, `wasi:config`); each tenant is an
//! object (`/{tenant}/`) that can override any of them, and every change is
//! pushed to the tenant's WebSockets. A flag is `true`, `false`, or
//! `{"percent": n}`: on for that share of users, the same users each time,
//! and the same ones still on when the share grows.
//!
//! - `GET /{tenant}/flags` (`?user=…` to evaluate a rollout for one user)
//! - `PUT /{tenant}/flags/{name}` (body: a value), `DELETE` for the default
//! - `/{tenant}/ws`: a WebSocket that gets the flags, then every change

use nanovm_sdk::{Request, Response, Result, config, kv, serde_json, ws};
use serde_json::{Map, Value, json};

nanovm_sdk::serve!(handle);

async fn handle(req: Request) -> Result {
    if let Some(events) = ws::events(&req) {
        let mut answer = ws::Answer::default();
        if events.contains(&ws::Event::Open) {
            answer.accept().subscribe("flags").text(&flags(None).await?.to_string());
        }
        return Ok(answer.into());
    }
    let path: Vec<&str> = req.path().trim_matches('/').split('/').collect();
    let name = match path.as_slice() {
        ["flags"] if req.method() == "GET" => return Ok(Response::json(flags(req.query("user")).await?.to_string())),
        ["flags", name] if defaults()?.contains_key(*name) => *name,
        _ => return Ok(Response::not_found()),
    };
    let key = format!("flag:{name}");
    match req.method() {
        "PUT" => match req.json::<Value>().ok().filter(valid) {
            Some(value) => kv::set(&key, value.to_string()).await?,
            None => return Ok(Response::text("a flag is true, false or {\"percent\": 0-100}\n").status(400)),
        },
        "DELETE" => kv::delete(&key).await?,
        _ => return Ok(Response::not_found()),
    }
    let now = flags(None).await?;
    // Watchers hear of the change once it has committed.
    ws::publish("flags", &now.to_string()).await?;
    Ok(Response::json(now[name].to_string()))
}

/// Every flag: `{"name": {"value", "source"}}`, with each value decided for
/// `user` if one is given.
async fn flags(user: Option<&str>) -> Result<Value> {
    let mut out = Map::new();
    for (name, default) in defaults()? {
        let (value, source) = match kv::get_text(&format!("flag:{name}")).await? {
            Some(set) => (serde_json::from_str(&set)?, "tenant"),
            None => (default, "default"),
        };
        let value = match (user, &value) {
            (Some(user), Value::Object(rollout)) => json!(bucket(&name, user) < rollout["percent"].as_u64().unwrap_or(0)),
            _ => value,
        };
        out.insert(name, json!({ "value": value, "source": source }));
    }
    Ok(Value::Object(out))
}

/// The flags the app's config names, with their defaults.
fn defaults() -> Result<Map<String, Value>> {
    let all = config::get_all().map_err(|e| nanovm_sdk::Error::new(format!("{e:?}")))?;
    let flags = all.into_iter().filter_map(|(k, v)| Some((k.strip_prefix("flag.")?.to_string(), v)));
    Ok(flags.filter_map(|(k, v)| Some((k, serde_json::from_str(&v).ok().filter(valid)?))).collect())
}

fn valid(value: &Value) -> bool {
    match value {
        Value::Bool(_) => true,
        Value::Object(o) => o.len() == 1 && o.get("percent").and_then(Value::as_u64).is_some_and(|p| p <= 100),
        _ => false,
    }
}

/// `user`'s bucket for `flag`, 0-99: FNV-1a, so it stays the same across
/// builds and restarts.
fn bucket(flag: &str, user: &str) -> u64 {
    let hash = format!("{flag}/{user}").bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    hash % 100
}
