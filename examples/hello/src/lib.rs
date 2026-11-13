use nanovm_sdk::{Body, Result, http, kv};

nanovm_sdk::serve!(handle);

async fn handle(_req: http::Request<Body>) -> Result {
    let visits = kv::increment("visits", 1).await?;
    Ok(http::Response::new(format!("{} has had {visits} visits\n", nanovm_sdk::id()).into()))
}
