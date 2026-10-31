//! Loopback OTLP sink for the demo: writes a bounded, atomic JSON snapshot.
//! Use an OpenTelemetry Collector for production storage and dashboards.
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full, Limited};

#[path = "../support/collector.rs"]
mod collector;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:4318").await?;
    let data = Arc::new(parking_lot::Mutex::new(collector::Collector::default()));
    let output = data.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            let snapshot = output.lock().snapshot();
            tokio::fs::write("target/demo-telemetry.tmp", serde_json::to_vec(&snapshot).unwrap()).await.unwrap();
            tokio::fs::rename("target/demo-telemetry.tmp", "target/demo-telemetry.json").await.unwrap();
        }
    });
    nanovm::server::serve_http1(listener, move |req| {
        let data = data.clone();
        async move {
            let path = req.uri().path().to_owned();
            let body = tokio::time::timeout(Duration::from_secs(2), Limited::new(req.into_body(), 1 << 20).collect()).await;
            if body.is_ok_and(|r| r.is_ok_and(|b| data.lock().ingest(&path, &b.to_bytes()).is_ok())) {
                http::Response::new(Full::new(bytes::Bytes::new()).map_err(|e| match e {}).boxed_unsync())
            } else {
                nanovm::object::error_response(400, "bad-otlp", "")
            }
        }
    }).await
}
