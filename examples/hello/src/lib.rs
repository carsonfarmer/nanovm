use nanovm_sdk::{Request, Response, Result, kv};

nanovm_sdk::serve!(handle);

async fn handle(_req: Request) -> Result {
    let visits = kv::increment("visits", 1).await?;
    Ok(Response::text(format!("{} has had {visits} visits\n", nanovm_sdk::id())))
}
