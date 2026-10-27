//! In-process load: requests go straight into [`Node::dispatch`], with no
//! sockets, so the numbers measure nanovm rather than a client. Shared by
//! the demo endpoints, the benchmark and the tests.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::Method;
use http_body_util::BodyExt;
use serde_json::{Value, json};

use crate::table::Node;

/// One response, fully read.
pub struct Reply {
    pub status: u16,
    /// `x-nanovm-error`: set when the host, not the guest, answered.
    pub error: Option<String>,
    pub body: String,
    pub elapsed: Duration,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }
}

/// Sends `method path` with `body` to object `id` of `app` and reads the reply.
pub async fn request(node: &Arc<Node>, method: &str, app: &str, id: &str, path: &str, body: &str) -> Reply {
    let t0 = Instant::now();
    let req = http::Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap_or(Method::GET))
        .uri(format!("http://localhost{path}"))
        .body(Bytes::from(body.to_string()))
        .unwrap();
    let resp = node.dispatch(app, id, req).await;
    let status = resp.status().as_u16();
    let error = resp
        .headers()
        .get("x-nanovm-error")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let body = match resp.into_body().collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => Bytes::from(format!("<body error: {e}>")),
    };
    Reply {
        status,
        error,
        body: String::from_utf8_lossy(&body).into_owned(),
        elapsed: t0.elapsed(),
    }
}

/// Latency and outcomes of many requests.
#[derive(Default)]
pub struct Run {
    pub elapsed: Duration,
    /// Per-request latency in µs; sorted once the run is complete.
    pub lat_us: Vec<u32>,
    pub status: BTreeMap<u16, usize>,
    /// Requests that didn't get a 200, by status and `x-nanovm-error`.
    pub failures: BTreeMap<String, usize>,
}

impl Run {
    pub fn add(&mut self, r: &Reply) {
        self.lat_us.push(r.elapsed.as_micros() as u32);
        *self.status.entry(r.status).or_default() += 1;
        if r.status != 200 {
            let reason = format!("{} {}", r.status, r.error.as_deref().unwrap_or("guest"));
            *self.failures.entry(reason).or_default() += 1;
        }
    }

    pub fn failed(&self) -> usize {
        self.failures.values().sum()
    }

    pub fn rps(&self) -> f64 {
        self.lat_us.len() as f64 / self.elapsed.as_secs_f64()
    }

    /// Latency at quantile `q`, in ms.
    pub fn pct(&self, q: f64) -> f64 {
        let i = ((self.lat_us.len() as f64 * q).ceil() as usize).clamp(1, self.lat_us.len().max(1)) - 1;
        self.lat_us.get(i).map_or(0.0, |&us| us as f64 / 1000.0)
    }

    pub fn json(&self) -> Value {
        json!({
            "requests": self.lat_us.len(),
            "failed": self.failed(),
            "failure_reasons": self.failures,
            "elapsed_ms": self.elapsed.as_secs_f64() * 1e3,
            "rps": self.rps().round(),
            "p50_ms": self.pct(0.50),
            "p99_ms": self.pct(0.99),
            "p999_ms": self.pct(0.999),
        })
    }
}

/// Runs `n` requests (or until `until`) from `conc` concurrent clients;
/// request `i` goes to object `id(i)`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    node: &Arc<Node>,
    app: &str,
    method: &str,
    path: &str,
    n: usize,
    until: Option<Instant>,
    conc: usize,
    id: impl Fn(usize) -> String + Send + Sync + 'static,
) -> Run {
    let next = Arc::new(AtomicUsize::new(0));
    let id = Arc::new(id);
    let t0 = Instant::now();
    let clients: Vec<_> = (0..conc.clamp(1, n.max(1)))
        .map(|_| {
            let (node, next, id) = (node.clone(), next.clone(), id.clone());
            let (app, method, path) = (app.to_string(), method.to_string(), path.to_string());
            tokio::spawn(async move {
                let mut run = Run::default();
                loop {
                    let i = next.fetch_add(1, Relaxed);
                    if i >= n || until.is_some_and(|d| Instant::now() >= d) {
                        return run;
                    }
                    run.add(&request(&node, &method, &app, &id(i), &path, "").await);
                }
            })
        })
        .collect();
    let mut total = Run::default();
    for c in clients {
        let run = c.await.unwrap_or_default();
        total.lat_us.extend(run.lat_us);
        for (k, v) in run.status {
            *total.status.entry(k).or_default() += v;
        }
        for (k, v) in run.failures {
            *total.failures.entry(k).or_default() += v;
        }
    }
    total.elapsed = t0.elapsed();
    total.lat_us.sort_unstable();
    total
}
