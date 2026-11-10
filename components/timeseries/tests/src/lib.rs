//! A minimal app over the time series component, driven by
//! crates/nanovm/tests/components.rs. Probes panic on violated expectations.
mod bindings {
    wit_bindgen::generate!({ path: "../wit", world: "client", generate_all });
}
use bindings::nanovm::timeseries::series::{Bucket, Error, Level, Point, Series};
use serde_json::{Value, json};
use util::wasip3::http::types::{ErrorCode, Request, Response};
util::wasip3::http::service::export!(App);
struct App;

const DAY: u64 = 86_400_000;

fn buckets(list: &[Bucket]) -> Value {
    list.iter().map(|b| json!([b.start_ms, b.count, b.sum, b.min, b.max])).collect()
}

fn points(list: &[Point]) -> Value {
    list.iter().map(|p| json!([p.time_ms, p.value])).collect()
}

fn fail(e: Error) -> Result<Response, ErrorCode> {
    util::json(409, json!({"error": format!("{e:?}")}).to_string())
}

/// Body lines `time value`; or `n` generated points from `start` every `every` ms, value = index.
fn parse(req: &util::Req) -> Vec<Point> {
    if let Some(n) = req.q("n") {
        let (start, every) = (req.q_u64("start", 0), req.q_u64("every", 1000));
        return (0..n.parse::<u64>().unwrap()).map(|i| Point { time_ms: start + i * every, value: i as f64 }).collect();
    }
    String::from_utf8(req.body.clone())
        .unwrap()
        .lines()
        .map(|l| {
            let (t, v) = l.split_once(' ').unwrap();
            Point { time_ms: t.parse().unwrap(), value: v.parse().unwrap() }
        })
        .collect()
}

impl util::wasip3::exports::http::handler::Guest for App {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let req = util::read_req(request).await;
        let s = Series::open(req.q("series").unwrap_or("cpu").into()).await.expect("open");
        let out = match req.path.as_str() {
            "/write" => match s.write(parse(&req)).await {
                Ok(()) => json!({}),
                Err(e) => return fail(e),
            },
            "/range" => {
                let got = s.range(req.q_u64("from", 0), req.q_u64("to", u64::MAX), req.q_u64("limit", 10_000) as u32);
                points(&got.await.expect("range"))
            }
            "/agg" => match s.aggregate(req.q_u64("from", 0), req.q_u64("to", 0), req.q_u64("step", 0)).await {
                Ok(b) => buckets(&b),
                Err(e) => return fail(e),
            },
            "/latest" => match s.latest().await.expect("latest") {
                Some(p) => json!([p.time_ms, p.value]),
                None => Value::Null,
            },
            "/trim" => {
                let level = match req.q("level") {
                    Some("hour") => Level::Hour,
                    Some("day") => Level::Day,
                    _ => Level::Raw,
                };
                s.trim(level, req.q_u64("before", 0)).await.expect("trim");
                json!({})
            }
            "/fail" => {
                s.write(parse(&req)).await.expect("write");
                return Err(ErrorCode::InternalError(Some("rollback probe".into())));
            }
            "/trap" => {
                s.write(parse(&req)).await.expect("write");
                panic!("rollback probe");
            }
            // Writes 1,000-point batches until the quota refuses one; the
            // probe and the test check that raw points and rollups agree.
            "/fill" => {
                let mut batches = 0u64;
                let err = loop {
                    let pts = (0..1000).map(|i| Point { time_ms: (batches * 1000 + i) * 1000, value: 1.0 }).collect();
                    match s.write(pts).await {
                        Ok(()) => batches += 1,
                        Err(e) => break e,
                    }
                };
                assert!(matches!(err, Error::Full), "{err:?}");
                json!({"batches": batches, "error": format!("{err:?}")})
            }
            "/contracts" => {
                for bad in ["", "../x", "a/b", &"n".repeat(65)] {
                    assert!(matches!(Series::open(bad.into()).await, Err(Error::InvalidName)), "{bad}");
                }
                let c = Series::open("contracts".into()).await.expect("open");
                assert!(c.latest().await.expect("latest").is_none());
                assert!(c.range(0, u64::MAX, 10).await.expect("empty").is_empty());
                assert!(c.aggregate(0, DAY, HOUR_MS).await.expect("empty").is_empty());
                let bad = |t, v| vec![Point { time_ms: t, value: v }];
                for pts in [vec![], bad(1 << 54, 1.0), bad(1, f64::NAN), bad(1, f64::INFINITY)] {
                    assert!(matches!(c.write(pts).await, Err(Error::InvalidArgument(_))));
                }
                assert!(matches!(c.range(0, 1, 0).await, Err(Error::InvalidArgument(_))));
                assert!(matches!(c.aggregate(0, 1, 0).await, Err(Error::InvalidArgument(_))));
                assert!(matches!(c.aggregate(0, 10_001, 1).await, Err(Error::InvalidArgument(_))));
                // Out of order, equal times, across a day boundary.
                let pts = [(DAY + 5, 7.0), (DAY - 1, -1.0), (DAY + 5, 8.0), (10, 2.5), (DAY + 1, 3.0)];
                c.write(pts.iter().map(|&(t, v)| Point { time_ms: t, value: v }).collect()).await.expect("write");
                let got: Vec<_> = c.range(0, u64::MAX, 100).await.expect("range").iter().map(|p| (p.time_ms, p.value)).collect();
                assert_eq!(got, [(10, 2.5), (DAY - 1, -1.0), (DAY + 1, 3.0), (DAY + 5, 7.0), (DAY + 5, 8.0)]);
                assert_eq!(c.range(DAY, DAY + 5, 100).await.expect("range").len(), 1);
                assert_eq!(c.range(0, u64::MAX, 2).await.expect("limit").len(), 2);
                let latest = c.latest().await.expect("latest").unwrap();
                assert_eq!((latest.time_ms, latest.value), (DAY + 5, 8.0));
                let days = c.aggregate(0, 2 * DAY, DAY).await.expect("days");
                assert_eq!(buckets(&days), json!([[0, 2, 1.5, -1.0, 2.5], [DAY, 3, 18.0, 3.0, 8.0]]));
                let two_days = c.aggregate(0, 2 * DAY, 2 * DAY).await.expect("2 days");
                assert_eq!(buckets(&two_days), json!([[0, 5, 19.5, -1.0, 8.0]]));
                let raw = c.aggregate(DAY, DAY + 10, 5).await.expect("5 ms");
                assert_eq!(buckets(&raw), json!([[DAY, 1, 3.0, 3.0, 3.0], [DAY + 5, 2, 15.0, 7.0, 8.0]]));
                let hours = c.aggregate(DAY - 1, DAY + 1, HOUR_MS).await.expect("hours");
                assert_eq!(buckets(&hours), json!([[DAY - HOUR_MS, 1, -1.0, -1.0, -1.0], [DAY, 3, 18.0, 3.0, 8.0]]));
                json!("ok")
            }
            _ => return util::text(404, "no such route"),
        };
        util::json(200, out.to_string())
    }
}

const HOUR_MS: u64 = 3_600_000;
