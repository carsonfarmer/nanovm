//! The example apps in `examples/`, each under `nanovm run` as a developer
//! runs it: what it does, that its state survives eviction (objects idle out
//! after 300 ms here) and a restart of the node, and that it refuses bad
//! input. Build them first: `tools/build-app.sh examples/<name> -o
//! examples/target/<name>.wasm` (CI does it for every example).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use reqwest::Method;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio_tungstenite::tungstenite::{Error, Message};

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// An example app under `nanovm run`, with its own data directory.
struct App {
    args: Vec<OsString>,
    addr: std::net::SocketAddr,
    lines: tokio::sync::mpsc::UnboundedReceiver<String>,
    child: tokio::process::Child,
    http: reqwest::Client,
}

impl App {
    /// Serves `examples/target/{name}.wasm` with `examples/{name}/nanovm.json`
    /// (if any) and `limits` changed, objects idling out after 300 ms.
    async fn start(name: &str, limits: Value) -> App {
        App::configured(name, limits, json!({})).await
    }

    /// [`App::start`], with `config` changed too.
    async fn configured(name: &str, limits: Value, config: Value) -> App {
        let wasm = repo().join(format!("examples/target/{name}.wasm"));
        assert!(wasm.exists(), "{} is missing: tools/build-app.sh examples/{name} -o {}", wasm.display(), wasm.display());
        let data = Path::new(env!("CARGO_TARGET_TMPDIR")).join("examples").join(name);
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        // The tests share one compile cache.
        let cache = Path::new(env!("CARGO_TARGET_TMPDIR")).join("cwasm-cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::os::unix::fs::symlink(cache, data.join("cache")).unwrap();
        let manifest = std::fs::read(repo().join(format!("examples/{name}/nanovm.json"))).unwrap_or(b"{}".into());
        let mut manifest: Value = serde_json::from_slice(&manifest).unwrap();
        manifest["limits"]["idle_timeout_ms"] = 300.into();
        // The most `nanovm run` allows: these tests check what the apps do,
        // and a busy disk's slow fsyncs mustn't turn into timeouts.
        manifest["limits"]["request_timeout_ms"] = 5000.into();
        manifest["limits"]["cpu_ms"] = 200.into();
        for (k, v) in limits.as_object().unwrap() {
            manifest["limits"][k] = v.clone();
        }
        for (k, v) in config.as_object().unwrap() {
            manifest["config"][k] = v.clone();
        }
        std::fs::write(data.join("nanovm.json"), manifest.to_string()).unwrap();
        let args = vec![wasm.into(), "--manifest".into(), data.join("nanovm.json").into(), "--data".into(), data.into()];
        App::spawn(args).await
    }

    async fn spawn(args: Vec<OsString>) -> App {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_nanovm"))
            .arg("run")
            .args(&args)
            .args(["--listen", "127.0.0.1:0"])
            .env_remove("RUST_BACKTRACE")
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stderr = tokio::io::BufReader::new(child.stderr.take().unwrap()).lines();
        let (tx, mut lines) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok(Some(line)) = stderr.next_line().await {
                eprintln!("nanovm run: {line}");
                let _ = tx.send(line);
            }
        });
        let line = next_line(&mut lines, " on http://").await;
        let addr = line.split("http://").nth(1).unwrap().split('/').next().unwrap().parse().unwrap();
        let http = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        App { args, addr, lines, child, http }
    }

    /// Stops the node and starts it again on the same data. It waits for the
    /// old process to be gone: until then it holds the app's storage lock.
    async fn restart(mut self) -> App {
        self.child.kill().await.unwrap();
        App::spawn(self.args).await
    }

    /// Waits until every object has idled out, and checks that `path`, asked
    /// next, starts its object afresh.
    async fn evicted(&mut self, path: &str) -> Reply {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        while self.lines.try_recv().is_ok() {}
        let reply = self.get(path).await;
        // Lines come a second or so late; a warm one is an earlier request's.
        let line = format!("GET {} →", path.split('?').next().unwrap());
        while !self.line(&line).await.ends_with("cold start") {}
        reply
    }

    /// The next line `nanovm run` prints that contains `text`.
    async fn line(&mut self, text: &str) -> String {
        next_line(&mut self.lines, text).await
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    async fn send(&self, method: &str, path: impl AsRef<str>, headers: &[(&str, &str)], body: impl Into<reqwest::Body>) -> Reply {
        let path = path.as_ref();
        let mut req = self.http.request(Method::from_bytes(method.as_bytes()).unwrap(), self.url(path)).body(body);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let res = req.send().await.unwrap_or_else(|e| panic!("{method} {path}: {e:?}"));
        let (status, headers) = (res.status().as_u16(), res.headers().clone());
        let body = res.bytes().await.unwrap().to_vec();
        Reply { status, headers, body }
    }

    async fn get(&self, path: impl AsRef<str>) -> Reply {
        self.send("GET", path, &[], "").await
    }

    async fn post(&self, path: impl AsRef<str>, body: impl Into<reqwest::Body>) -> Reply {
        self.send("POST", path, &[], body).await
    }

    /// A WebSocket, or the refusal's status.
    async fn open(&self, path: &str) -> Result<Ws, u16> {
        match tokio_tungstenite::connect_async(format!("ws://{}{path}", self.addr)).await {
            Ok((ws, _)) => Ok(ws),
            Err(Error::Http(res)) => Err(res.status().as_u16()),
            Err(e) => panic!("{path}: {e}"),
        }
    }
}

async fn next_line(lines: &mut tokio::sync::mpsc::UnboundedReceiver<String>, text: &str) -> String {
    loop {
        let line = tokio::time::timeout(Duration::from_secs(60), lines.recv()).await;
        let line = line.unwrap_or_else(|_| panic!("no line with {text:?} in time")).expect("nanovm run exited");
        if line.contains(text) {
            return line;
        }
    }
}

struct Reply {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("{e}: {} {}", self.status, self.text()))
    }

    fn header(&self, name: &str) -> &str {
        self.headers.get(name).map(|v| v.to_str().unwrap()).unwrap_or("")
    }
}

impl std::fmt::Debug for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status, self.text())
    }
}

/// The next data message, as JSON.
async fn next(ws: &mut Ws) -> Value {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(10), ws.next()).await.expect("a message in time");
        match m.expect("the socket is open").expect("a frame") {
            Message::Text(t) => return serde_json::from_str(&t).unwrap_or_else(|e| panic!("{e}: {t}")),
            Message::Ping(_) | Message::Pong(_) => continue,
            m => panic!("not text: {m:?}"),
        }
    }
}

async fn say(ws: &mut Ws, text: &str) {
    ws.send(Message::text(text)).await.unwrap();
}

/// examples/shortener: a link per object, a redirect that counts clicks per
/// day, codes minted by one object asking another to take a link.
#[tokio::test(flavor = "multi_thread")]
async fn shortener_redirects_and_counts_clicks() {
    let mut app = App::start("shortener", json!({})).await;
    let made = app.post("/new/", "https://example.com/a?b=c").await;
    assert_eq!(made.status, 201, "{made:?}");
    let code = made.json()["code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 7);
    for _ in 0..2 {
        let go = app.get(&format!("/{code}")).await;
        assert_eq!((go.status, go.header("location")), (302, "https://example.com/a?b=c"));
    }
    let stats = app.get(&format!("/{code}/stats")).await.json();
    assert_eq!((&stats["url"], &stats["clicks"]), (&json!("https://example.com/a?b=c"), &json!(2)), "{stats}");
    let days = stats["days"].as_object().unwrap();
    assert!(days.len() == 1 && days.values().all(|d| d == 2), "{stats}");
    assert_eq!(app.send("HEAD", &format!("/{code}"), &[], "").await.status, 302, "HEAD doesn't count");

    // A chosen code is taken once; bad links and unknown codes are refused.
    assert_eq!(app.post("/docs/", "https://example.com/docs").await.status, 201);
    assert_eq!(app.post("/docs/", "https://evil.example").await.status, 409);
    assert_eq!(app.get("/docs").await.header("location"), "https://example.com/docs");
    for bad in ["", "javascript:alert(1)", "https://a b", &format!("https://{}", "a".repeat(2048))] {
        assert_eq!(app.post("/new/", bad.to_string()).await.status, 400, "{bad}");
    }
    assert_eq!(app.get("/nope").await.status, 404);
    assert_eq!(app.post("/nope/stats", "").await.status, 404);

    // Links outlive their instances and the node.
    let stats = app.evicted(&format!("/{code}/stats")).await.json();
    assert_eq!(stats["clicks"], 2, "{stats}");
    let app = app.restart().await;
    assert_eq!(app.get(&format!("/{code}")).await.status, 302);
    assert_eq!(app.get(&format!("/{code}/stats")).await.json()["clicks"], 3);
    assert_eq!(app.post("/docs/", "https://evil.example").await.status, 409);
}

/// examples/ratelimit: a token bucket per key object. Concurrent takes
/// can't overspend it, it refills by the clock, and a spent bucket stays
/// spent across eviction and restarts (an in-memory limiter would reset).
#[tokio::test(flavor = "multi_thread")]
async fn ratelimit_buckets_are_exact_and_durable() {
    let mut app = App::start("ratelimit", json!({})).await;
    let fresh = app.get("/a/").await.json();
    assert!(fresh["tokens"] == 10.0 && fresh["capacity"] == 10.0, "the config's defaults: {fresh}");

    // Twenty at once against five tokens: exactly five get one.
    assert_eq!(app.send("PUT", "/burst/", &[], r#"{"capacity":5,"per_second":0.001}"#).await.status, 204);
    let takes = futures::future::join_all((0..20).map(|_| app.post("/burst/take", ""))).await;
    let mut remaining: Vec<_> = takes.iter().filter(|r| r.status == 200).map(|r| r.json()["remaining"].as_f64().unwrap()).collect();
    remaining.sort_by(f64::total_cmp);
    assert_eq!(remaining, [0.0, 1.0, 2.0, 3.0, 4.0]);
    let denied = takes.iter().find(|r| r.status != 200).unwrap();
    assert_eq!(takes.iter().filter(|r| r.status == 429).count(), 15);
    assert!(denied.header("retry-after").parse::<u64>().unwrap() > 900, "{denied:?}");
    for bad in ["/burst/take?n=0", "/burst/take?n=6", "/burst/take?n=x1"] {
        assert_eq!(app.post(bad, "").await.status, 400, "{bad}");
    }
    for bad in ["", r#"{"capacity":0,"per_second":1}"#, r#"{"capacity":5,"per_second":-1}"#] {
        assert_eq!(app.send("PUT", "/x/", &[], bad).await.status, 400, "{bad}");
    }

    // It refills by the clock: once denied, waiting as long as it says is enough.
    assert_eq!(app.send("PUT", "/refill/", &[], r#"{"capacity":2,"per_second":4}"#).await.status, 204);
    let denied = loop {
        let take = app.post("/refill/take", "").await;
        if take.status == 429 {
            break take.json();
        }
    };
    let wait = denied["retry_after_ms"].as_u64().unwrap();
    assert!((1..=250).contains(&wait), "{denied}");
    tokio::time::sleep(Duration::from_millis(wait)).await;
    assert_eq!(app.post("/refill/take", "").await.status, 200);

    // A spent bucket stays spent.
    assert!(app.evicted("/burst/").await.json()["tokens"].as_f64().unwrap() < 1.0);
    assert_eq!(app.post("/burst/take", "").await.status, 429);
    let app = app.restart().await;
    assert_eq!(app.post("/burst/take", "").await.status, 429);
    assert_eq!(app.post("/a/take?n=10", "").await.json()["remaining"], 0.0);
}

/// examples/locks: one object per lock, so one holder at a time however
/// many ask at once; leases expire by themselves, and fencing tokens only
/// grow, across releases, expiries, eviction and restarts.
#[tokio::test(flavor = "multi_thread")]
async fn locks_have_one_holder_and_growing_fencing_tokens() {
    let mut app = App::start("locks", json!({})).await;
    let tries = futures::future::join_all((0..10).map(|i| app.post(format!("/db/acquire?owner=w{i}&ttl_ms=60000"), ""))).await;
    let won: Vec<_> = tries.iter().filter(|r| r.status == 200).map(|r| r.json()).collect();
    assert_eq!(won.len(), 1, "{tries:?}");
    let (holder, token) = (won[0]["owner"].as_str().unwrap().to_string(), won[0]["token"].as_i64().unwrap());
    assert_eq!(token, 1);
    for lost in tries.iter().filter(|r| r.status != 200) {
        assert_eq!((lost.status, lost.json()["owner"].as_str()), (409, Some(holder.as_str())), "told who holds it");
    }
    assert_eq!(app.get("/db/").await.json()["owner"], holder.as_str());

    // Only the holder, with its token, renews or releases.
    let other = if holder == "w0" { "w1" } else { "w0" };
    assert_eq!(app.post(&format!("/db/release?owner={other}&token=1"), "").await.status, 409);
    assert_eq!(app.post(&format!("/db/release?owner={holder}&token=2"), "").await.status, 409);
    let renewed = app.post(&format!("/db/renew?owner={holder}&token=1&ttl_ms=120000"), "").await.json();
    assert!(renewed["expires_at"].as_u64() > won[0]["expires_at"].as_u64(), "{renewed}");
    assert_eq!(app.post(&format!("/db/release?owner={holder}&token=1"), "").await.status, 204);
    assert_eq!(app.get("/db/").await.json(), Value::Null);
    let next = app.post(&format!("/db/acquire?owner={other}"), "").await.json();
    assert_eq!(next["token"], 2, "a new grant, a larger token");
    assert_eq!(app.post(&format!("/db/acquire?owner={other}"), "").await.json()["token"], 2, "acquiring again renews");
    for bad in ["/db/acquire", "/db/acquire?owner=a&ttl_ms=0", "/db/acquire?owner=a&ttl_ms=x"] {
        assert_eq!(app.post(bad, "").await.status, 400, "{bad}");
    }

    // A lease ends by itself; its old holder is then fenced off.
    assert_eq!(app.post("/job/acquire?owner=slow&ttl_ms=200", "").await.json()["token"], 1);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(app.post("/job/acquire?owner=fast", "").await.json()["token"], 2);
    let late = app.post("/job/renew?owner=slow&token=1", "").await;
    assert_eq!((late.status, late.json()["owner"].as_str()), (409, Some("fast")));

    // Leases and tokens are durable.
    assert_eq!(app.evicted("/db/").await.json()["owner"], other);
    let app = app.restart().await;
    assert_eq!(app.post("/job/acquire?owner=slow", "").await.status, 409);
    assert_eq!(app.post("/job/release?owner=fast&token=2", "").await.status, 204);
    assert_eq!(app.post("/job/acquire?owner=slow", "").await.json()["token"], 3);
}

/// examples/polls: a poll per object, one vote per voter cookie (voting
/// again moves the vote), results pushed to WebSockets once a vote commits.
#[tokio::test(flavor = "multi_thread")]
async fn polls_count_each_voter_once_and_push_results() {
    let mut app = App::start("polls", json!({})).await;
    let votes = |r: &Value| -> Vec<u64> { r["options"].as_array().unwrap().iter().map(|o| o["votes"].as_u64().unwrap()).collect() };
    let poll = r#"{"question":"Lunch?","options":["pizza","sushi","tacos"]}"#;
    assert_eq!(app.post("/lunch/vote?option=0", "").await.status, 404, "no poll yet");
    assert_eq!(app.send("PUT", "/lunch/", &[], poll).await.status, 201);
    assert_eq!(app.send("PUT", "/lunch/", &[], poll).await.status, 409);
    for bad in ["{}", r#"{"question":"q","options":["one"]}"#, r#"{"question":" ","options":["a","b"]}"#] {
        assert_eq!(app.send("PUT", "/bad/", &[], bad).await.status, 400, "{bad}");
    }
    let mut watch = app.open("/lunch/ws").await.unwrap();
    assert_eq!(votes(&next(&mut watch).await), [0, 0, 0]);

    // A first vote gets a voter cookie; voting again with it moves the vote.
    let first = app.post("/lunch/vote?option=0", "").await;
    let cookie = first.header("set-cookie").split(';').next().unwrap().to_string();
    assert!(cookie.starts_with("voter=") && cookie.len() == 22, "{cookie}");
    assert_eq!((votes(&first.json()), &first.json()["you"]), (vec![1, 0, 0], &json!(0)));
    assert_eq!(votes(&next(&mut watch).await), [1, 0, 0]);
    let me = [("cookie", cookie.as_str())];
    assert_eq!(votes(&app.send("POST", "/lunch/vote?option=1", &me, "").await.json()), [0, 1, 0]);
    assert_eq!(votes(&next(&mut watch).await), [0, 1, 0]);
    assert_eq!(votes(&app.send("POST", "/lunch/vote?option=1", &me, "").await.json()), [0, 1, 0], "the same vote again");
    assert_eq!(app.send("GET", "/lunch/", &me, "").await.json()["you"], 1);
    assert!(app.get("/lunch/").await.json().get("you").is_none());
    for bad in ["/lunch/vote?option=3", "/lunch/vote?option=-1", "/lunch/vote"] {
        assert_eq!(app.post(bad, "").await.status, 400, "{bad}");
    }

    // Twenty new voters at once: every vote counts, once.
    let crowd = (0..20).map(|i| format!("voter={i:016x}"));
    let app_ = &app;
    let crowd: Vec<_> = crowd.map(|c| async move { app_.send("POST", "/lunch/vote?option=2", &[("cookie", &c)], "").await }).collect();
    assert!(futures::future::join_all(crowd).await.iter().all(|r| r.status == 200));
    let now = app.get("/lunch/").await.json();
    assert_eq!((votes(&now), &now["voters"]), (vec![0, 1, 20], &json!(21)));
    let mut seen = vec![];
    while seen != [0, 1, 20] {
        seen = votes(&next(&mut watch).await);
    }

    // Votes and voters outlive the instance and the node; sockets outlive the instance.
    assert_eq!(votes(&app.evicted("/lunch/").await.json()), [0, 1, 20]);
    app.send("POST", "/lunch/vote?option=0", &me, "").await;
    assert_eq!(votes(&next(&mut watch).await), [1, 0, 20]);
    drop(watch);
    let app = app.restart().await;
    assert_eq!(votes(&app.send("POST", "/lunch/vote?option=2", &me, "").await.json()), [0, 0, 21]);
    assert_eq!(app.get("/lunch/").await.json()["voters"], 21);
}

/// examples/leaderboard: SQLite in an object. Best scores per player, pages
/// of the board that add up to the whole of it, ranks with ties, and a
/// database that outlives eviction and restarts.
#[tokio::test(flavor = "multi_thread")]
async fn leaderboard_pages_and_ranks_from_sqlite() {
    let mut app = App::start("leaderboard", json!({})).await;
    // 150 players, scores with ties, in two batches.
    let score = |i: usize| (i * 37 % 100) as i64;
    let players: Vec<_> = (0..150).map(|i| json!({"player": format!("p{i:03}"), "score": score(i)})).collect();
    for batch in players.chunks(100) {
        let r = app.post("/arcade/scores", Value::from(batch.to_vec()).to_string()).await;
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(r.json().as_array().unwrap().len(), batch.len());
    }
    // Best first; ties to whoever got there first (the first batch), then by name.
    let mut expected: Vec<(i64, usize, String)> = (0..150).map(|i| (-score(i), i / 100, format!("p{i:03}"))).collect();
    expected.sort();
    let mut board = vec![];
    for offset in (0..).step_by(40) {
        let page = app.get(format!("/arcade/top?limit=40&offset={offset}")).await.json();
        let page = page.as_array().unwrap();
        board.extend(page.iter().map(|e| (e["rank"].as_u64().unwrap(), e["player"].as_str().unwrap().to_string())));
        if page.len() < 40 {
            break;
        }
    }
    assert_eq!(board.len(), 150);
    for (i, ((rank, player), (neg, _, name))) in board.iter().zip(&expected).enumerate() {
        assert_eq!(player, name, "position {i}");
        let better = expected.iter().filter(|e| e.0 < *neg).count() as u64;
        assert_eq!(*rank, better + 1, "{player}: competition ranking");
    }
    assert_eq!(app.get("/arcade/top?limit=1000").await.json().as_array().unwrap().len(), 100, "pages are at most 100");

    // A worse score doesn't replace a best one; a better one does.
    let top = &board[0].1;
    let mine = app.get(format!("/arcade/players/{top}")).await.json();
    assert_eq!((&mine["rank"], &mine["score"]), (&json!(1), &json!(99)));
    let worse = app.post("/arcade/scores", json!({"player": top, "score": 1}).to_string()).await.json();
    assert_eq!(worse[0]["score"], 99);
    let up = app.post("/arcade/scores", r#"{"player":"p149","score":1000}"#).await.json();
    assert_eq!((&up[0]["rank"], &up[0]["score"]), (&json!(1), &json!(1000)));
    assert_eq!(app.get(format!("/arcade/players/{top}")).await.json()["rank"], 2);
    assert_eq!(app.send("DELETE", "/arcade/players/p149", &[], "").await.status, 204);
    assert_eq!(app.get("/arcade/players/p149").await.status, 404);
    for bad in ["{}", r#"{"player":"","score":1}"#, r#"{"player":"a","score":1.5}"#, "nope"] {
        assert_eq!(app.post("/arcade/scores", bad).await.status, 400, "{bad}");
    }
    let many: Vec<_> = (0..101).map(|i| json!({"player": format!("x{i}"), "score": 1})).collect();
    assert_eq!(app.post("/arcade/scores", Value::from(many).to_string()).await.status, 400);
    assert_eq!(app.get("/other/top").await.json(), json!([]), "boards have their own databases");

    // The database is the object's: it survives eviction and restarts.
    let top = app.evicted("/arcade/top?limit=3").await.json();
    assert_eq!(top[0]["player"], board[0].1.as_str());
    let app = app.restart().await;
    assert_eq!(app.get("/arcade/top?limit=200").await.json().as_array().unwrap().len(), 100);
    assert_eq!(app.get(format!("/arcade/players/{}", board[0].1)).await.json()["rank"], 1);
}

/// examples/shop: a failed checkout (a declined card, after the stock was
/// taken and the order written) rolls all of it back; concurrent checkouts
/// never oversell; orders and stock survive eviction and restarts.
#[tokio::test(flavor = "multi_thread")]
async fn shop_checkout_is_all_or_nothing() {
    let mut app = App::start("shop", json!({})).await;
    let put = async |path: &str, body: Value| app.send("PUT", path, &[], body.to_string()).await;
    assert_eq!(put("/s/products/widget", json!({"name":"Widget","price":250,"stock":5})).await.status, 204);
    assert_eq!(put("/s/products/gadget", json!({"name":"Gadget","price":1000,"stock":1})).await.status, 204);
    assert_eq!(put("/s/products/bad", json!({"name":"Bad","price":-1,"stock":1})).await.status, 400);
    assert_eq!(put("/s/carts/a/items/widget", json!({"qty":2})).await.status, 200);
    let cart = put("/s/carts/a/items/gadget", json!({"qty":1})).await.json();
    assert_eq!(cart["total"], 1500, "{cart}");
    assert_eq!(put("/s/carts/a/items/nothing", json!({"qty":1})).await.status, 404);
    let stock = async || -> Vec<(String, i64)> {
        let products = app.get("/s/products").await.json();
        products.as_array().unwrap().iter().map(|p| (p["sku"].as_str().unwrap().into(), p["stock"].as_i64().unwrap())).collect()
    };
    let before = stock().await;

    // Declined after the writes: the request fails and nothing it did stays.
    let declined = app.post("/s/carts/a/checkout", r#"{"card":"4000000000000002"}"#).await;
    assert_eq!((declined.status, declined.header("x-nanovm-error")), (500, "guest-error"), "{declined:?}");
    assert!(declined.text().contains("payment declined"), "{declined:?}");
    assert_eq!(stock().await, before, "the stock it took is back");
    assert_eq!(app.get("/s/carts/a").await.json()["total"], 1500, "the cart is intact");
    assert_eq!(app.get("/s/orders/1").await.status, 404, "no order");

    // Paid: stock taken, order written, cart emptied, card charged once.
    let paid = app.post("/s/carts/a/checkout", r#"{"card":"4242424242424242"}"#).await;
    assert_eq!(paid.status, 201, "{paid:?}");
    let id = paid.json()["order"].as_i64().unwrap();
    assert_eq!(stock().await, [("gadget".into(), 0), ("widget".into(), 3)]);
    assert_eq!(app.get("/s/carts/a").await.json()["items"], json!([]));
    let order = app.get(format!("/s/orders/{id}")).await.json();
    assert_eq!((&order["total"], order["items"].as_array().unwrap().len()), (&json!(1500), 2), "{order}");
    let charge = app.get(format!("/payments/charges?key=s/{id}")).await.json();
    assert_eq!((&charge["amount"], &charge["card"]), (&json!(1500), &json!("4242424242424242")));
    assert_eq!(app.get("/payments/charges?key=s/2").await.status, 404, "the declined attempt charged nothing");

    // Short stock is refused before anything is written.
    put("/s/carts/b/items/gadget", json!({"qty":1})).await;
    let short = app.post("/s/carts/b/checkout", r#"{"card":"4242424242424242"}"#).await;
    assert_eq!((short.status, &short.json()["short"]), (409, &json!(["gadget"])));
    assert_eq!(app.post("/s/carts/empty/checkout", r#"{"card":"4242"}"#).await.status, 400);

    // Five carts want the last three widgets at once: three get them.
    for c in 0..5 {
        put(&format!("/s/carts/c{c}/items/widget"), json!({"qty":1})).await;
    }
    let app_ = &app;
    let rush = (0..5).map(|c| async move { app_.post(format!("/s/carts/c{c}/checkout"), r#"{"card":"4242424242424242"}"#).await });
    let rush = futures::future::join_all(rush).await;
    let mut statuses: Vec<u16> = rush.iter().map(|r| r.status).collect();
    statuses.sort();
    assert_eq!(statuses, [201, 201, 201, 409, 409], "{rush:?}");
    assert_eq!(stock().await[1], ("widget".into(), 0));

    // Orders and stock are durable.
    assert_eq!(app.evicted(&format!("/s/orders/{id}")).await.json()["total"], 1500);
    let app = app.restart().await;
    let products = app.get("/s/products").await.json();
    assert_eq!((&products[0]["stock"], &products[1]["stock"]), (&json!(0), &json!(0)));
    assert_eq!(app.get("/s/orders/4").await.json()["cart"].as_str().map(|c| c.starts_with('c')), Some(true));
    assert_eq!(app.get("/s/orders/5").await.status, 404);
}

/// examples/jobs: idempotent enqueues, leases whose visibility timeout is
/// seen by the next lease, acks fenced by the lease, a worker object pulling
/// through calls, retries into the dead letters, and a queue that survives
/// eviction and restarts.
#[tokio::test(flavor = "multi_thread")]
async fn jobs_lease_retry_and_dead_letter() {
    let mut app = App::start("jobs", json!({})).await;
    let add = async |queue: &str, payload: Value, key: Option<&str>| {
        let headers: Vec<(&str, &str)> = key.map(|k| ("idempotency-key", k)).into_iter().collect();
        app.send("POST", format!("/{queue}/jobs"), &headers, payload.to_string()).await
    };
    let first = add("mail", json!({"text": "hello"}), Some("k1")).await;
    assert_eq!((first.status, &first.json()["state"]), (201, &json!("ready")), "{first:?}");
    let again = add("mail", json!({"text": "other"}), Some("k1")).await;
    assert_eq!((again.status, &again.json()["id"], &again.json()["payload"]), (200, &first.json()["id"], &json!({"text": "hello"})));
    for payload in [json!({"text": "b"}), json!({"text": "c", "fail": true}), json!({"text": "d"})] {
        assert_eq!(add("mail", payload, None).await.status, 201);
    }
    assert_eq!(app.post("/mail/jobs", "not json").await.status, 400);

    // A lease that runs out makes its job visible to the next lease, and only
    // the latest lease can ack it.
    let id = add("solo", json!({"text": "hello"}), None).await.json()["id"].clone();
    let one = app.post("/solo/lease?visibility_ms=200", "").await.json();
    assert_eq!((&one["id"], &one["attempts"], &one["state"]), (&id, &json!(1), &json!("leased")));
    assert_eq!(app.post("/solo/lease", "").await.status, 204, "leased, so not visible");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let two = app.post("/solo/lease?visibility_ms=60000", "").await.json();
    assert_eq!((&two["id"], &two["attempts"]), (&id, &json!(2)), "the same job, again");
    let ack = |lease: &Value| format!("/solo/jobs/{id}/ack?lease={}", lease.as_str().unwrap());
    assert_eq!(app.post(ack(&one["lease"]), "late").await.status, 409, "its lease ran out");
    assert_eq!(app.post(ack(&two["lease"]), "HELLO").await.status, 204);
    assert_eq!(app.get(format!("/solo/jobs/{id}")).await.json()["result"], "HELLO");

    // A worker object pulls the rest through calls; the failing job is
    // retried until it is dead.
    let worked = app.post("/w1/work?queue=mail&retry_ms=0", "").await;
    assert_eq!(worked.json(), json!({"done": 3, "failed": 3}), "{worked:?}");
    assert_eq!(app.get("/mail/").await.json(), json!({"ready": 0, "leased": 0, "done": 3, "dead": 1}));
    let dead = app.get("/mail/dead").await.json();
    assert_eq!((&dead[0]["payload"]["text"], &dead[0]["attempts"], &dead[0]["error"]), (&json!("c"), &json!(3), &json!("could not do it")));
    assert_eq!(app.post("/mail/lease", "").await.status, 204, "nothing left to lease");
    let dead_id = dead[0]["id"].as_i64().unwrap();
    assert_eq!(app.post(format!("/mail/dead/{dead_id}/retry"), "").await.status, 204);
    assert_eq!(app.post(format!("/mail/dead/{dead_id}/retry"), "").await.status, 404);

    assert_eq!(app.post("/mail/lease?visibility_ms=60000", "").await.json()["id"], dead_id, "retried");

    // A job leased on its last attempt whose lease runs out is dead too.
    let last = add("once", json!({"text": "e"}), None).await.json()["id"].clone();
    for _ in 0..3 {
        assert_eq!(app.post("/once/lease?visibility_ms=1", "").await.json()["id"], last);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(app.post("/once/lease", "").await.status, 204);
    let gone = app.get(format!("/once/jobs/{last}")).await.json();
    assert_eq!((&gone["state"], &gone["error"]), (&json!("dead"), &json!("its last lease ran out")));

    // The queue is durable: a leased job stays leased.
    let counts = app.evicted("/mail/").await.json();
    assert_eq!(counts, json!({"ready": 0, "leased": 1, "done": 3, "dead": 0}));
    let app = app.restart().await;
    assert_eq!(app.get("/mail/").await.json(), counts);
    assert_eq!(add_after_restart(&app).await, 200, "keys are remembered");
}

async fn add_after_restart(app: &App) -> u16 {
    app.send("POST", "/mail/jobs", &[("idempotency-key", "k1")], r#"{"text":"x"}"#).await.status
}

/// A streamed response's chunks, each with when it arrived, and whether the
/// body ended cleanly.
async fn chunks(app: &App, path: &str) -> (Vec<(std::time::Instant, String)>, bool) {
    let mut res = app.http.get(app.url(path)).send().await.unwrap();
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    let mut out = vec![];
    loop {
        match res.chunk().await {
            Ok(Some(c)) => out.push((std::time::Instant::now(), String::from_utf8_lossy(&c).into_owned())),
            Ok(None) => return (out, true),
            Err(_) => return (out, false),
        }
    }
}

/// examples/events: Server-Sent Events replayed from a log with a reconnect
/// hint, and a long-lived stream written after the handler returned. While
/// it streams, its object serves nothing else (other objects are not held
/// up); what it changes after answering commits when it ends, and rolls back
/// if it outlives `post_return_timeout_ms`.
#[tokio::test(flavor = "multi_thread")]
async fn events_stream_after_the_handler_returns() {
    let mut app = App::start("events", json!({"post_return_timeout_ms": 2000})).await;
    for (i, text) in ["one", "two\nlines", "three"].iter().enumerate() {
        assert_eq!(app.post("/feed/events", text.to_string()).await.json()["id"], i + 1);
    }
    let all = app.get("/feed/events").await.text();
    assert_eq!(all, "retry: 1000\n\nid: 1\ndata: one\n\nid: 2\ndata: two\ndata: lines\n\nid: 3\ndata: three\n\n");
    let since = app.send("GET", "/feed/events", &[("last-event-id", "2")], "").await;
    assert_eq!((since.header("content-type"), since.text().as_str()), ("text/event-stream", "retry: 1000\n\nid: 3\ndata: three\n\n"));
    assert_eq!(app.get("/feed/events?after=3").await.text(), "retry: 1000\n\n");

    // A countdown is streamed: its ticks arrive 200 ms apart, after the
    // handler has returned.
    let (got, clean) = chunks(&app, "/cd/countdown?from=5&every_ms=200").await;
    let text: String = got.iter().map(|(_, c)| c.as_str()).collect();
    assert!(clean && text.ends_with("event: done\ndata: liftoff\n\n"), "{text}");
    assert_eq!(text.matches("event: tick").count(), 5);
    assert!(got.last().unwrap().0 - got[0].0 >= Duration::from_millis(800), "streamed, not buffered: {got:?}");
    assert_eq!(app.get("/cd/").await.json()["countdowns"], 1, "its write after answering committed");

    // While it streams its object waits; other objects don't.
    let stream = tokio::spawn({
        let (http, url) = (app.http.clone(), app.url("/cd/countdown?from=6&every_ms=200"));
        async move {
            let mut res = http.get(url).send().await.unwrap();
            while res.chunk().await.unwrap().is_some() {}
            std::time::Instant::now()
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let other = app.get("/elsewhere/").await;
    let other_done = std::time::Instant::now();
    let same = app.post("/cd/events", "queued").await;
    let same_done = std::time::Instant::now();
    let ended = stream.await.unwrap();
    assert_eq!((other.status, same.status), (200, 201));
    assert!(other_done < ended && same_done >= ended, "the stream's own object waited for it");
    assert_eq!(app.get("/cd/").await.json()["countdowns"], 2);

    // A stream that outlives post_return_timeout_ms is cut, its object
    // stopped, and what it changed after answering rolled back.
    let (got, clean) = chunks(&app, "/cd/countdown?from=30&every_ms=200").await;
    let text: String = got.iter().map(|(_, c)| c.as_str()).collect();
    assert!(!text.contains("done") && text.matches("event: tick").count() < 30, "cut short: clean={clean} {text}");
    app.line("deadline exceeded while post-return").await;
    assert_eq!(app.get("/cd/").await.json()["countdowns"], 2, "the cut stream's write rolled back");

    // The log outlives the instance and the node.
    assert_eq!(app.evicted("/feed/events?after=2").await.text(), "retry: 1000\n\nid: 3\ndata: three\n\n");
    let app = app.restart().await;
    assert_eq!(app.get("/feed/").await.json(), json!({"events": 3, "countdowns": 0}));
    assert_eq!(app.get("/cd/").await.json(), json!({"events": 1, "countdowns": 2}));
}

/// examples/files: uploads within the disk quota (507 past it, the old file
/// kept), downloads streamed with ranges, ETags and HEAD, and files that
/// survive eviction and restarts.
#[tokio::test(flavor = "multi_thread")]
async fn files_upload_within_quota_and_serve_ranges() {
    let mut app = App::start("files", json!({"post_return_timeout_ms": 5000})).await;
    let kib = |n: usize, seed: u8| -> Vec<u8> { (0..n << 10).map(|i| (i % 251) as u8 ^ seed).collect() };
    let a = kib(700, 0);
    let put = app.send("PUT", "/b/files/a.bin", &[("content-type", "application/x-test")], a.clone()).await;
    assert_eq!(put.status, 201, "{put:?}");
    let etag = put.header("etag").to_string();

    let full = app.get("/b/files/a.bin").await;
    assert_eq!((full.status, full.header("content-type"), full.header("accept-ranges")), (200, "application/x-test", "bytes"));
    assert_eq!((full.header("content-length"), full.body == a), ("716800", true));
    let part = async |range: &str| app.send("GET", "/b/files/a.bin", &[("range", range)], "").await;
    let first = part("bytes=0-99").await;
    assert_eq!((first.status, first.header("content-range"), &first.body[..]), (206, "bytes 0-99/716800", &a[..100]));
    assert_eq!(part("bytes=-10").await.body, a[a.len() - 10..]);
    let tail = part("bytes=716700-").await;
    assert_eq!((tail.header("content-range"), &tail.body[..]), ("bytes 716700-716799/716800", &a[716700..]));
    let middle = part("bytes=65530-65545").await;
    assert_eq!(middle.body, a[65530..65546], "across a piece boundary");
    let beyond = part("bytes=716800-").await;
    assert_eq!((beyond.status, beyond.header("content-range")), (416, "bytes */716800"));
    assert_eq!(part("bytes=0-1,5-6").await.status, 200, "several ranges: the whole file");
    assert_eq!(app.send("GET", "/b/files/a.bin", &[("if-none-match", &etag)], "").await.status, 304);
    let head = app.send("HEAD", "/b/files/a.bin", &[], "").await;
    assert_eq!((head.status, head.header("content-length"), head.body.len()), (200, "716800", 0));

    // The bucket has 2 MiB: a third 700 KiB file doesn't fit, and is
    // refused without touching what is there.
    assert_eq!(app.send("PUT", "/b/files/b.bin", &[], kib(700, 1)).await.status, 201);
    let refused = app.send("PUT", "/b/files/c.bin", &[], kib(700, 2)).await;
    assert_eq!(refused.status, 507, "{refused:?}");
    let refused = app.send("PUT", "/b/files/a.bin", &[], kib(700, 3)).await;
    assert_eq!(refused.status, 507, "a replacement needs room for both while it is written");
    assert_eq!(app.get("/b/files/a.bin").await.body == a, true, "the old file is intact");
    let list = app.get("/b/").await.json();
    assert_eq!((list["bytes"].as_u64(), list["files"].as_array().unwrap().len()), (Some(1400 << 10), 2));
    let big = app.send("PUT", "/b/files/big.bin", &[], kib(1100, 0)).await;
    assert_eq!((big.status, big.header("x-nanovm-error")), (413, "request-too-large"), "max_request_body_kb");
    for bad in ["/b/files/.hidden", "/b/files/a%2Fb", "/b/nope"] {
        assert_eq!(app.send("PUT", bad, &[], "x").await.status, 404, "{bad}");
    }
    assert_eq!(app.send("DELETE", "/b/files/b.bin", &[], "").await.status, 204);
    assert_eq!(app.send("PUT", "/b/files/c.bin", &[], kib(700, 2)).await.status, 201, "deleting freed the space");
    let small = app.send("PUT", "/b/files/a.bin", &[], "small now").await;
    assert_ne!(small.header("etag"), etag);

    // Files are in the volume: they survive eviction and the node.
    assert_eq!(app.evicted("/b/files/a.bin").await.text(), "small now");
    let app = app.restart().await;
    let c = app.send("GET", "/b/files/c.bin", &[("range", "bytes=-4")], "").await;
    assert_eq!((c.status, c.body.as_slice()), (206, &kib(700, 2)[(700 << 10) - 4..]));
    assert_eq!(app.get("/b/").await.json()["bytes"], (700 << 10) + 9);
}

/// examples/site: embedded files with ETags and Cache-Control (304 when
/// unchanged), and an API in the same component whose state is durable.
#[tokio::test(flavor = "multi_thread")]
async fn site_serves_embedded_files_and_its_api() {
    let mut app = App::start("site", json!({})).await;
    let page = app.get("/www/").await;
    assert_eq!((page.status, page.header("content-type"), page.header("cache-control")), (200, "text/html; charset=utf-8", "no-cache"));
    assert!(page.text().contains(r#"<script src="app.js">"#));
    let etag = page.header("etag").to_string();
    assert_eq!(app.send("GET", "/www/", &[("if-none-match", &etag)], "").await.status, 304);
    assert_eq!(app.send("GET", "/www/", &[("if-none-match", "\"other\", *")], "").await.status, 304);
    assert_eq!(app.send("GET", "/www/", &[("if-none-match", "\"other\"")], "").await.status, 200);
    let css = app.get("/www/style.css").await;
    assert_eq!((css.header("content-type"), css.header("cache-control")), ("text/css; charset=utf-8", "public, max-age=3600"));
    assert_ne!(css.header("etag"), etag);
    let head = app.send("HEAD", "/www/app.js", &[], "").await;
    let js = app.get("/www/app.js").await;
    assert_eq!((head.status, head.header("content-length"), head.body.len()), (200, js.header("content-length"), 0));
    assert_eq!(app.get("/www/nope.html").await.status, 404);
    assert_eq!(app.post("/www/", "").await.status, 404);
    let root = app.get("/").await;
    assert_eq!((root.status, root.header("x-nanovm-error")), (400, "bad-object-id"), "no site at / (W-601)");

    // The API beside it.
    assert_eq!(app.get("/www/api/likes").await.json(), json!({"likes": 0}));
    for n in 1..=3 {
        assert_eq!(app.post("/www/api/likes", "").await.json()["likes"], n);
    }
    assert_eq!(app.get("/www/api/likes").await.header("cache-control"), "no-store");
    assert_eq!(app.evicted("/www/api/likes").await.json()["likes"], 3);
    let app = app.restart().await;
    assert_eq!(app.get("/www/api/likes").await.json()["likes"], 3);
    assert_eq!(app.get("/www/").await.header("etag"), etag, "the same bytes, the same ETag");
}

/// examples/presence: joins, leaves and typing pushed to a room's sockets;
/// two tabs are one person; presence survives eviction, and lapses after a
/// restart (whose sockets never said goodbye) unless refreshed.
#[tokio::test(flavor = "multi_thread")]
async fn presence_tracks_who_is_here() {
    let mut app = App::configured("presence", json!({}), json!({"stale_ms": "2000"})).await;
    // The next event of `kind`, skipping others (a socket may hear its own join).
    async fn event(ws: &mut Ws, kind: &str) -> Value {
        loop {
            let m = next(ws).await;
            if m["type"] == kind {
                return m;
            }
        }
    }
    let who = async |app: &App| app.get("/lounge/who").await.json()["names"].clone();
    assert_eq!(app.open("/lounge/ws").await.err(), Some(400), "a socket needs a name");
    let mut ann = app.open("/lounge/ws?name=ann").await.unwrap();
    assert_eq!(event(&mut ann, "here").await["names"], json!(["ann"]));
    let mut bob = app.open("/lounge/ws?name=bob").await.unwrap();
    assert_eq!(event(&mut bob, "here").await["names"], json!(["ann", "bob"]));
    assert_eq!(event(&mut ann, "join").await["name"], "bob");
    say(&mut bob, "typing").await;
    assert_eq!(event(&mut ann, "typing").await["name"], "bob");
    assert_eq!(who(&app).await, json!(["ann", "bob"]));

    // A second tab is the same person: closing one leaves them here.
    let mut ann2 = app.open("/lounge/ws?name=ann").await.unwrap();
    assert_eq!(event(&mut ann2, "here").await["names"], json!(["ann", "bob"]));
    ann2.close(None).await.unwrap();
    bob.close(None).await.unwrap();
    assert_eq!(event(&mut ann, "leave").await["name"], "bob", "bob left; ann's closed tab said nothing");
    assert_eq!(who(&app).await, json!(["ann"]));

    // The room's instance comes and goes under ann's socket.
    assert_eq!(app.evicted("/lounge/who").await.json()["names"], json!(["ann"]));
    let mut cy = app.open("/lounge/ws?name=cy").await.unwrap();
    assert_eq!(event(&mut ann, "join").await["name"], "cy");
    say(&mut cy, "typing").await;
    assert_eq!(event(&mut ann, "typing").await["name"], "cy");

    // A restart drops every socket without a word: presence lapses unless
    // refreshed, and a fresh socket that pings stays.
    drop((ann, cy));
    let app = app.restart().await;
    let mut dee = app.open("/lounge/ws?name=dee").await.unwrap();
    event(&mut dee, "here").await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    say(&mut dee, "ping").await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(who(&app).await, json!(["dee"]));
}

/// examples/tictactoe: a lobby pairs players through calls to game objects
/// (never three to a game, even all at once); each game validates moves and
/// pushes the board to its sockets; games survive eviction and restarts.
#[tokio::test(flavor = "multi_thread")]
async fn tictactoe_lobby_pairs_players_and_games_judge_moves() {
    let mut app = App::start("tictactoe", json!({})).await;
    let matched = async |p: &str| app.post(format!("/lobby/match?player={p}"), "").await.json();
    let ann = matched("ann").await;
    let game = ann["game"].as_str().unwrap().to_string();
    assert_eq!(ann["you"], "x");
    assert_eq!(matched("ann").await, ann, "asking again keeps her place");
    let mut ann_ws = app.open(&format!("/{game}/ws?player=ann")).await.unwrap();
    assert_eq!(next(&mut ann_ws).await["o"], Value::Null, "waiting for an opponent");
    let bob = matched("bob").await;
    assert_eq!((bob["game"].as_str(), &bob["you"]), (Some(game.as_str()), &json!("o")));
    assert_eq!(next(&mut ann_ws).await["o"], "bob", "pushed when bob joined");
    let mut bob_ws = app.open(&format!("/{game}/ws?player=bob")).await.unwrap();
    assert_eq!(next(&mut bob_ws).await["board"], ".........");

    // Moves are checked by the game: turn, square, and the end.
    say(&mut bob_ws, "0").await;
    assert_eq!(next(&mut bob_ws).await["error"], "not your turn");
    say(&mut ann_ws, "4").await;
    for ws in [&mut ann_ws, &mut bob_ws] {
        assert_eq!(next(ws).await["board"], "....x....");
    }
    say(&mut bob_ws, "4").await;
    assert_eq!(next(&mut bob_ws).await["error"], "that square is taken");
    say(&mut bob_ws, "nine").await;
    assert_eq!(next(&mut bob_ws).await["error"], "a square is 0 to 8");
    let mv = async |p: &str, s: u8| app.post(format!("/{game}/move?player={p}&square={s}"), "").await;
    for (p, s) in [("bob", 0), ("ann", 1), ("bob", 2), ("ann", 7)] {
        assert_eq!(mv(p, s).await.status, 200, "{p} {s}");
    }
    let mut last = Value::Null;
    while last["result"].is_null() {
        last = next(&mut ann_ws).await;
    }
    assert_eq!((&last["board"], &last["result"]), (&json!("oxo.x..x."), &json!("x")), "ann's column");
    assert_eq!(mv("bob", 8).await.json()["error"], "the game is over");
    assert_eq!(app.post("/elsewhere/join", "eve").await.status, 404, "no game there to join");

    // Ten players at once: five games of exactly two.
    let app_ = &app;
    let rush = (0..10).map(|i| async move { app_.post(format!("/lobby/match?player=p{i}"), "").await.json() });
    let rush = futures::future::join_all(rush).await;
    let mut games = std::collections::BTreeMap::<String, Vec<String>>::new();
    for m in &rush {
        games.entry(m["game"].as_str().unwrap().into()).or_default().push(m["you"].as_str().unwrap().into());
    }
    assert!(games.len() == 5 && games.values().all(|seats| seats.len() == 2 && seats.contains(&"x".into())), "{games:?}");

    // Games outlive their instances (sockets too) and the node.
    let state = app.evicted(&format!("/{game}/")).await.json();
    assert_eq!(state["result"], "x");
    let app = app.restart().await;
    assert_eq!(app.get(format!("/{game}/")).await.json(), state);
}

/// examples/flags: defaults from the app's config, overrides per tenant
/// object pushed live to its sockets, stable and monotonic percentage
/// rollouts, all durable.
#[tokio::test(flavor = "multi_thread")]
async fn flags_override_defaults_per_tenant_live() {
    let mut app = App::start("flags", json!({})).await;
    let flags = app.get("/acme/flags").await.json();
    assert_eq!(flags["dark_mode"], json!({"value": true, "source": "default"}));
    assert_eq!(flags["search_v2"]["value"], json!({"percent": 25}));
    let mut watch = app.open("/acme/ws").await.unwrap();
    assert_eq!(next(&mut watch).await, flags);

    let set = app.send("PUT", "/acme/flags/new_checkout", &[], "true").await;
    assert_eq!(set.json(), json!({"value": true, "source": "tenant"}));
    assert_eq!(next(&mut watch).await["new_checkout"]["value"], true, "pushed live");
    assert_eq!(app.get("/other/flags").await.json()["new_checkout"]["value"], false, "tenants are their own");
    assert_eq!(app.send("PUT", "/acme/flags/nope", &[], "true").await.status, 404, "only flags the app defines");
    for bad in ["maybe", r#"{"percent":101}"#, r#"{"percent":5,"x":1}"#, "1"] {
        assert_eq!(app.send("PUT", "/acme/flags/dark_mode", &[], bad).await.status, 400, "{bad}");
    }

    // A rollout is per user, the same each time, and only grows.
    let on = async |app: &App| -> std::collections::BTreeSet<usize> {
        let mut on = std::collections::BTreeSet::new();
        for batch in (0..100).collect::<Vec<_>>().chunks(25) {
            let asks = batch.iter().map(|u| async move { (*u, app.get(format!("/acme/flags?user=u{u}")).await.json()) });
            for (u, f) in futures::future::join_all(asks).await {
                if f["search_v2"]["value"] == true {
                    on.insert(u);
                }
            }
        }
        on
    };
    let quarter = on(&app).await;
    assert_eq!(on(&app).await, quarter, "the same users each time");
    app.send("PUT", "/acme/flags/search_v2", &[], r#"{"percent":50}"#).await;
    let half = on(&app).await;
    assert!(quarter.is_subset(&half) && quarter.len() < half.len() && half.len() < 100, "{} {}", quarter.len(), half.len());
    app.send("PUT", "/acme/flags/search_v2", &[], r#"{"percent":0}"#).await;
    assert!(on(&app).await.is_empty());
    while next(&mut watch).await["search_v2"]["value"] != json!({"percent": 0}) {}

    // Back to the default, and durable.
    assert_eq!(app.send("DELETE", "/acme/flags/search_v2", &[], "").await.json()["source"], "default");
    assert_eq!(next(&mut watch).await["search_v2"]["source"], "default");
    assert_eq!(app.evicted("/acme/flags").await.json()["new_checkout"]["value"], true);
    let app = app.restart().await;
    let flags = app.get("/acme/flags").await.json();
    assert_eq!((&flags["new_checkout"]["source"], &flags["search_v2"]["source"]), (&json!("tenant"), &json!("default")));
}

/// examples/wiki: revisions in SQLite, edits that would overwrite someone
/// else's refused, Markdown rendered with raw HTML escaped, diffs between
/// revisions, all durable.
#[tokio::test(flavor = "multi_thread")]
async fn wiki_keeps_every_revision_and_refuses_lost_updates() {
    let mut app = App::start("wiki", json!({})).await;
    let save = async |rev: &str, author: &str, body: &str| {
        app.send("PUT", format!("/w/pages/Home?author={author}"), &[("if-match", rev)], body.to_string()).await
    };
    assert_eq!(save("0", "ann", "# Home\n\nWelcome.\n").await.json()["rev"], 1);
    assert_eq!(save("1", "bob", "# Home\n\nWelcome, all.\n\n<script>alert(1)</script>\n").await.json()["rev"], 2);
    // Ann saves an edit of revision 1: bob's would be lost, so it is refused.
    let stale = save("1", "ann", "# Home\n\nHi.\n").await;
    assert_eq!((stale.status, &stale.json()["rev"], stale.header("etag")), (409, &json!(2), "\"2\""));
    assert_eq!(app.send("PUT", "/w/pages/Home", &[], "x").await.status, 428, "an edit names its base");
    assert_eq!(save("0", "eve", "again").await.status, 409, "the page exists");
    assert_eq!(app.send("PUT", "/w/pages/a%20b", &[("if-match", "0")], "x").await.status, 404);

    let page = app.get("/w/pages/Home").await;
    assert_eq!((page.header("etag"), page.header("content-type")), ("\"2\"", "text/html; charset=utf-8"));
    assert!(page.text().contains("<h1>Home</h1>") && page.text().contains("<p>Welcome, all.</p>"), "{}", page.text());
    assert!(page.text().contains("&lt;script&gt;") && !page.text().contains("<script>"), "raw HTML is shown, not run");
    assert_eq!(app.get("/w/pages/Home?rev=1&raw=1").await.text(), "# Home\n\nWelcome.\n");
    assert_eq!(app.get("/w/pages/Home?rev=9").await.status, 404);
    let history = app.get("/w/pages/Home/history").await.json();
    let authors: Vec<_> = history.as_array().unwrap().iter().map(|r| (r["rev"].as_i64().unwrap(), r["author"].as_str().unwrap())).collect();
    assert_eq!(authors, [(2, "bob"), (1, "ann")]);
    let diff = app.get("/w/pages/Home/diff?from=1&to=2").await.text();
    assert!(diff.starts_with("--- Home@1\n+++ Home@2\n") && diff.contains("-Welcome.\n+Welcome, all.\n"), "{diff}");
    assert_eq!(app.get("/w/pages/Home/diff?from=1&to=3").await.status, 404);

    // Every revision survives eviction and restarts.
    assert_eq!(app.evicted("/w/").await.json(), json!([{"page": "Home", "rev": 2}]));
    let app = app.restart().await;
    assert_eq!(app.get("/w/pages/Home?rev=1&raw=1").await.text(), "# Home\n\nWelcome.\n");
    let next = app.send("PUT", "/w/pages/Home?author=ann", &[("if-match", "2")], "# Home\n").await;
    assert_eq!(next.json()["rev"], 3);
}

/// examples/wiki: Markdown's own links and images, not only its raw HTML,
/// can run script: `[x](javascript:…)` is an `href` a click runs. The
/// destinations served are read as a browser reads them (case, tabs and
/// newlines inside, controls around), and only http, https, mailto and
/// relative ones are kept; the page's CSP refuses script besides.
#[tokio::test(flavor = "multi_thread")]
async fn review_wiki_does_not_serve_executable_markdown_links() {
    let app = App::start("wiki", json!({})).await;
    let hostile = "# Links\n\n\
        [a](javascript:alert(1)) [b](JaVaScRiPt:alert(2)) [c](java&#x09;script:alert(3)) [d](&#x20;javascript:alert(4))\n\
        <javascript:alert(5)> [e][ref] ![f](javascript:alert(6)) [g](vbscript:msgbox(7))\n\
        [h](data:text/html;base64,PHNjcmlwdD5hbGVydCg4KTwvc2NyaXB0Pg==) [i](&#106;avascript:alert(9)) [j](java\nscript:alert(10))\n\n\
        [ref]: javascript:alert(11)\n\n\
        [ok](https://example.com/a?b=1&c=2) [rel](/w/pages/Other) [mail](mailto:ann@example.com) [frag](#top) [page](Other)\n\
        ![img](https://example.com/i.png) [plain](http://example.com/) [colon](./a:b)\n";
    let saved = app.send("PUT", "/w/pages/Links", &[("if-match", "0")], hostile).await;
    assert_eq!(saved.status, 201, "{saved:?}");
    let page = app.get("/w/pages/Links").await;
    let html = page.text();

    // Every href and src, as the browser will resolve its scheme: entities
    // decoded, tabs and newlines dropped, controls and spaces trimmed.
    let mut urls = vec![];
    for attr in ["href=\"", "src=\""] {
        for (i, _) in html.match_indices(attr) {
            let value = &html[i + attr.len()..];
            let value = &value[..value.find('"').unwrap()];
            let value = value.replace("&quot;", "\"").replace("&#39;", "'").replace("&#x27;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&");
            urls.push(value);
        }
    }
    let scheme = |url: &str| -> Option<String> {
        let url: String = url.trim_matches(|c: char| c <= ' ').chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
        let end = url.find(|c: char| !(c.is_ascii_alphanumeric() || "+-.".contains(c)))?;
        (url[end..].starts_with(':') && url.starts_with(|c: char| c.is_ascii_alphabetic())).then(|| url[..end].to_ascii_lowercase())
    };
    for url in &urls {
        if let Some(s) = scheme(url) {
            assert!(["http", "https", "mailto"].contains(&s.as_str()), "an executable destination {url:?} in {html}");
        }
    }
    for kept in ["https://example.com/a?b=1&c=2", "/w/pages/Other", "mailto:ann@example.com", "#top", "Other", "https://example.com/i.png", "http://example.com/", "./a:b"] {
        assert!(urls.iter().any(|u| u == kept), "{kept} is kept: {urls:?}");
    }
    let csp = page.header("content-security-policy");
    assert!(csp.contains("default-src 'none'") && !csp.contains("script-src"), "no script, whatever the page holds: {csp:?}");
}

/// examples/notes: an FTS5 index per person, kept in step by the same
/// statements that save notes; ranked, highlighted prefix search that
/// treats query syntax as words; durable.
#[tokio::test(flavor = "multi_thread")]
async fn notes_search_their_own_full_text_index() {
    let mut app = App::start("notes", json!({})).await;
    let add = async |user: &str, title: &str, body: &str| {
        let r = app.post(format!("/{user}/notes"), json!({"title": title, "body": body}).to_string()).await;
        assert_eq!(r.status, 201, "{r:?}");
        r.json()["id"].as_i64().unwrap()
    };
    let soup = add("ann", "Soup", "Leek and potato soup: simmer the leeks slowly.").await;
    let trip = add("ann", "Trip to Lisbon", "Book the train; pack light. Try the soup at the market.").await;
    add("ann", "Taxes", "File by April.").await;
    add("bob", "Soup", "Bob's own soup.").await;
    let ids = |found: &Value| -> Vec<i64> { found.as_array().unwrap().iter().map(|n| n["id"].as_i64().unwrap()).collect() };

    let found = app.get("/ann/search?q=soup").await.json();
    assert_eq!(ids(&found), [soup, trip], "a title match ranks first");
    assert!(found[1]["snippet"].as_str().unwrap().contains("[soup]"), "{found}");
    assert_eq!(ids(&app.get("/ann/search?q=simmering%20leek").await.json()), [soup], "stems and prefixes, all words");
    assert_eq!(ids(&app.get("/ann/search?q=lisb").await.json()), [trip]);
    assert_eq!(app.get("/ann/search?q=%22NEAR(soup").await.status, 200, "query syntax is only words");
    for bad in ["/ann/search", "/ann/search?q=%20%21"] {
        assert_eq!(app.get(bad).await.status, 400, "{bad}");
    }

    // Edits and deletes keep the index in step.
    let edit = json!({"title": "Trip to Porto", "body": "Book the train."}).to_string();
    assert_eq!(app.send("PUT", format!("/ann/notes/{trip}"), &[], edit).await.status, 204);
    assert_eq!(ids(&app.get("/ann/search?q=soup").await.json()), [soup]);
    assert_eq!(app.send("DELETE", format!("/ann/notes/{soup}"), &[], "").await.status, 204);
    assert_eq!(app.get("/ann/search?q=soup").await.json(), json!([]));
    assert_eq!(app.get(format!("/ann/notes/{soup}")).await.status, 404);
    assert_eq!(app.get("/ann/notes/x").await.status, 404);
    assert_eq!(app.get("/bob/search?q=soup").await.json().as_array().unwrap().len(), 1, "bob's notes are his");

    let porto = app.evicted(&format!("/ann/notes/{trip}")).await.json();
    assert_eq!(porto["title"], "Trip to Porto");
    let app = app.restart().await;
    assert_eq!(ids(&app.get("/ann/search?q=porto%20train").await.json()), [trip]);
}

/// examples/analytics: batched ingest in one statement, a per-day rollup
/// kept by a trigger, distinct users, CSV export, durable.
#[tokio::test(flavor = "multi_thread")]
async fn analytics_rolls_events_up_per_day() {
    let mut app = App::start("analytics", json!({})).await;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let yesterday = now - 86_400_000;
    let batch = json!([
        {"name": "view", "path": "/", "user": "u1"},
        {"name": "view", "path": "/a", "user": "u1"},
        {"name": "view", "path": "/b", "user": "u2"},
        {"name": "click", "path": "/b", "user": "u1"},
        {"name": "view", "path": "/", "user": "u3", "at": yesterday},
        {"name": "view", "path": "/", "user": "u4", "at": yesterday},
    ]);
    assert_eq!(app.post("/blog/events", batch.to_string()).await.json(), json!({"accepted": 6}));
    let all = app.get("/blog/days?from=2000-01-01&to=2999-12-31").await.json();
    let rows: Vec<(String, i64, i64)> = all.as_array().unwrap().iter()
        .map(|r| (r["name"].as_str().unwrap().into(), r["events"].as_i64().unwrap(), r["users"].as_i64().unwrap())).collect();
    assert_eq!(rows, [("view".into(), 2, 2), ("click".into(), 1, 1), ("view".into(), 3, 2)], "{all}");
    let (yday, today) = (all[0]["day"].as_str().unwrap().to_string(), all[1]["day"].as_str().unwrap().to_string());
    assert!(yday < today);
    assert_eq!(app.get("/blog/days").await.json().as_array().unwrap().len(), 2, "today by default");
    let csv = app.get(format!("/blog/days.csv?from={yday}&to={today}")).await;
    assert_eq!(csv.header("content-type"), "text/csv; charset=utf-8");
    assert_eq!(csv.text(), format!("day,name,events,users\n{yday},view,2,2\n{today},click,1,1\n{today},view,3,2\n"));

    assert_eq!(app.post("/blog/events", r#"{"name":"view","user":"u5"}"#).await.status, 202, "one event alone");
    let month_ago = now - 31 * 86_400_000;
    for bad in [json!({"name": "a b"}), json!({"name": "view", "at": month_ago}), json!("view")] {
        assert_eq!(app.post("/blog/events", bad.to_string()).await.status, 400, "{bad}");
    }
    let many: Vec<_> = (0..101).map(|_| json!({"name": "x"})).collect();
    assert_eq!(app.post("/blog/events", Value::from(many).to_string()).await.status, 400);
    assert_eq!(app.get("/other/days").await.json(), json!([]), "sites are their own");

    let later = app.evicted(&format!("/blog/days?from={today}&to={today}")).await.json();
    assert_eq!((&later[1]["events"], &later[1]["users"]), (&json!(4), &json!(3)));
    let app = app.restart().await;
    assert_eq!(app.get(format!("/blog/days.csv?from={yday}&to={yday}")).await.text(), format!("day,name,events,users\n{yday},view,2,2\n"));
}

/// examples/reminders: no alarms, so an external tick (or the client's own
/// polling) delivers due reminders, each exactly once, pushed to the
/// owner's sockets; pending reminders survive eviction and restarts.
#[tokio::test(flavor = "multi_thread")]
async fn reminders_fire_once_on_a_tick_or_a_poll() {
    let mut app = App::start("reminders", json!({})).await;
    let tick = async || app.post("/clock/tick", "").await.json()["fired"].as_i64().unwrap();
    // Bob polls: what is due is his, once, and a tick doesn't fire it again.
    let now = app.post("/bob/reminders", r#"{"text":"now","in_ms":0}"#).await;
    assert_eq!(now.status, 201, "{now:?}");
    let due = app.get("/bob/due").await.json();
    assert_eq!((due.as_array().unwrap().len(), &due[0]["text"]), (1, &json!("now")));
    assert_eq!(app.get("/bob/due").await.json(), json!([]));
    assert_eq!(tick().await, 0, "bob's was polled already");

    // The tick delivers ann's to her socket once it is due, and only once.
    let mut ann_ws = app.open("/ann/ws").await.unwrap();
    assert_eq!(app.post("/ann/reminders", r#"{"text":"tea","in_ms":300}"#).await.status, 201);
    assert_eq!(app.post("/ann/reminders", r#"{"text":"later","in_ms":3600000}"#).await.status, 201);
    assert_eq!(app.post("/ann/reminders", r#"{"text":""}"#).await.status, 400);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let ticks = futures::future::join_all([tick(), tick()]).await;
    assert_eq!(ticks.iter().sum::<i64>(), 1, "two ticks at once fire it once: {ticks:?}");
    assert_eq!(next(&mut ann_ws).await["text"], "tea");
    let list = app.get("/ann/reminders").await.json();
    let fired: Vec<_> = list.as_array().unwrap().iter().map(|r| (r["text"].as_str().unwrap(), r["fired"].as_bool().unwrap())).collect();
    assert_eq!(fired, [("tea", true), ("later", false)]);
    assert_eq!(app.post("/ann/fire?id=1", "").await.status, 204, "firing again does nothing");

    // Pending reminders and the clock's schedule are durable.
    assert_eq!(app.evicted("/ann/reminders").await.json()[1]["fired"], false);
    let app = app.restart().await;
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 - 1;
    assert_eq!(app.post("/cy/reminders", json!({"text": "overdue", "at": at}).to_string()).await.status, 201);
    assert_eq!(app.post("/clock/tick", "").await.json()["fired"], 1);
    assert_eq!(app.get("/cy/reminders").await.json()[0]["fired"], true);
}

/// examples/todos: two devices sync offline edits through one object per
/// person; edits to different fields merge, edits to the same field
/// conflict (the first to sync wins and the other is told), retried syncs
/// apply nothing twice, all durable.
#[tokio::test(flavor = "multi_thread")]
async fn todos_sync_offline_edits_and_report_conflicts() {
    let mut app = App::start("todos", json!({})).await;
    let sync = async |since: u64, ops: Value| app.post("/ann/sync", json!({"since": since, "ops": ops}).to_string()).await.json();
    let titles = |r: &Value| -> Vec<(String, String, bool)> {
        r["changes"].as_array().unwrap().iter()
            .map(|t| (t["id"].as_str().unwrap().into(), t["title"].as_str().unwrap().into(), t["done"].as_bool().unwrap())).collect()
    };
    let laptop = sync(0, json!([{"id": "l1", "todo": "t1", "base": 0, "title": "milk"}, {"id": "l2", "todo": "t2", "base": 0, "title": "eggs"}])).await;
    assert_eq!((laptop["version"].as_u64(), laptop["conflicts"].as_array().unwrap().len()), (Some(2), 0));
    let phone = sync(0, json!([])).await;
    assert_eq!(titles(&phone), [("t1".into(), "milk".into(), false), ("t2".into(), "eggs".into(), false)]);

    // Offline, both edit t1, different fields: they merge.
    let laptop = sync(2, json!([{"id": "l3", "todo": "t1", "base": 2, "title": "oat milk"}])).await;
    let phone = sync(2, json!([{"id": "p1", "todo": "t1", "base": 2, "done": true}])).await;
    assert_eq!((titles(&phone), &phone["conflicts"]), (vec![("t1".into(), "oat milk".into(), true)], &json!([])));
    // Both rename t2: the laptop synced first, so the phone is told.
    sync(laptop["version"].as_u64().unwrap(), json!([{"id": "l4", "todo": "t2", "base": 2, "title": "eggs x12"}])).await;
    let ops = json!([{"id": "p2", "todo": "t2", "base": 2, "title": "brown eggs"}]);
    let phone = sync(4, ops.clone()).await;
    assert_eq!(phone["conflicts"], json!([{"op": "p2", "todo": "t2", "fields": ["title"]}]));
    assert_eq!(titles(&phone).last().unwrap().1, "eggs x12", "the list kept the first");
    // The phone's answer was lost and it retries: nothing applies twice.
    let again = sync(4, ops).await;
    assert_eq!((&again["version"], &again["conflicts"]), (&phone["version"], &json!([])));

    // A delete wins over a later edit based on before it.
    let v = again["version"].as_u64().unwrap();
    sync(v, json!([{"id": "l5", "todo": "t1", "base": v, "delete": true}])).await;
    let late = sync(v, json!([{"id": "p3", "todo": "t1", "base": v, "title": "soy milk"}])).await;
    assert_eq!(late["conflicts"][0]["fields"], json!(["title"]));
    assert_eq!(app.post("/ann/sync", r#"{"since":0,"ops":[{"id":"x y","todo":"t","base":0}]}"#).await.status, 400);
    assert_eq!(app.post("/bob/sync", r#"{"since":0}"#).await.json()["changes"], json!([]), "lists are their own");

    let all = app.evicted("/ann/sync").await;
    assert_eq!(all.status, 404, "a GET is not a sync");
    let app = app.restart().await;
    let all = app.post("/ann/sync", r#"{"since":0}"#).await.json();
    let t1 = all["changes"].as_array().unwrap().iter().find(|t| t["id"] == "t1").unwrap().clone();
    assert_eq!((&t1["deleted"], &t1["title"], &all["version"]), (&json!(true), &json!("oat milk"), &late["version"]));
}

/// An example app on an in-process dev-mode node, its requests dispatched
/// straight to it: for apps whose egress must reach a server on loopback,
/// which `nanovm run` (like a production node) refuses.
/// `manifest` with each table of `patch` (`limits`, `config`, `env`) changed.
fn patched(mut manifest: Value, patch: &Value) -> Value {
    for (table, changes) in patch.as_object().unwrap() {
        for (k, v) in changes.as_object().unwrap() {
            manifest[table][k] = v.clone();
        }
    }
    manifest
}

struct Local {
    node: std::sync::Arc<nanovm::table::Node>,
    cfg: nanovm::config::NodeConfig,
    app: nanovm::config::AppConfig,
    wasm: Vec<u8>,
}

impl Local {
    /// `examples/target/{name}.wasm` with its manifest, `patch`'s `limits`,
    /// `config` and `env` changed, `patch`'s `secrets` given as an operator
    /// gives them, and objects idling out after 300 ms.
    fn start(name: &str, patch: Value) -> Local {
        let wasm = std::fs::read(repo().join(format!("examples/target/{name}.wasm")))
            .unwrap_or_else(|_| panic!("build it: tools/build-app.sh examples/{name} -o examples/target/{name}.wasm"));
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
        let data = root.join("examples-local").join(name);
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(root.join("cwasm-cache")).unwrap();
        std::os::unix::fs::symlink(root.join("cwasm-cache"), data.join("cache")).unwrap();
        let mut manifest: Value = serde_json::from_slice(&std::fs::read(repo().join(format!("examples/{name}/nanovm.json"))).unwrap()).unwrap();
        manifest["limits"]["idle_timeout_ms"] = 300.into();
        let mut patch = patch;
        let secrets = serde_json::from_value(patch.as_object_mut().unwrap().remove("secrets").unwrap_or_default()).unwrap_or_default();
        let manifest = patched(manifest, &patch);
        let spec: nanovm::registry::AppSpec = serde_json::from_value(manifest).unwrap();
        let cfg = nanovm::config::NodeConfig { data_dir: data, max_live_objects: 16, memory_reservation_mb: 64, dev: true, ..Default::default() };
        let app = nanovm::config::AppConfig {
            name: name.into(),
            digest: "local".into(),
            env: spec.env,
            config: spec.config,
            secrets,
            limits: spec.limits,
            worker: Default::default(),
        };
        nanovm::engine::preflight(&cfg).unwrap();
        let node = nanovm::table::Node::new(cfg.clone(), &app, &wasm, None).unwrap();
        Local { node, cfg, app, wasm }
    }

    async fn send(&self, method: &str, path: &str, headers: &[(&str, &str)], body: impl Into<bytes::Bytes>) -> Reply {
        let (id, rest) = nanovm::table::route(path).unwrap();
        let mut req = http::Request::builder().method(method).uri(format!("http://x{}", if rest.is_empty() { "/" } else { rest }));
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let res = self.node.dispatch(id, req.body(body.into()).unwrap()).await;
        let (parts, body) = res.into_parts();
        let body = http_body_util::BodyExt::collect(body).await.unwrap().to_bytes().to_vec();
        Reply { status: parts.status.as_u16(), headers: parts.headers, body }
    }

    /// Waits for every object to idle out.
    async fn evicted(&self) {
        let t0 = std::time::Instant::now();
        while self.node.live_objects() > 0 {
            assert!(t0.elapsed() < Duration::from_secs(20), "objects never idled out");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Shuts the node down and starts another on the same data.
    async fn restart(self) -> Local {
        self.node.shutdown().await;
        drop(self.node);
        let node = nanovm::table::Node::new(self.cfg.clone(), &self.app, &self.wasm, None).unwrap();
        Local { node, ..self }
    }
}

/// A server on loopback that records what it is sent and answers each
/// request with the next status of `script` (then 200).
async fn upstream(script: Vec<u16>, delay: Duration) -> (String, std::sync::Arc<std::sync::Mutex<Vec<(http::HeaderMap, String)>>>) {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
    let script = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(script)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let log = seen.clone();
    tokio::spawn(nanovm::server::serve_http1(listener, move |req: http::Request<hyper::body::Incoming>| {
        let (log, script) = (log.clone(), script.clone());
        async move {
            let (parts, body) = req.into_parts();
            let body = http_body_util::BodyExt::collect(body).await.unwrap().to_bytes();
            log.lock().unwrap().push((parts.headers, String::from_utf8_lossy(&body).into_owned()));
            tokio::time::sleep(delay).await;
            match script.lock().unwrap().pop_front().unwrap_or(200) {
                200 => nanovm::server::ok_json(json!({"ok": true})),
                status => nanovm::object::error_response(status, "scripted", "failing on purpose"),
            }
        }
    }));
    (addr, seen)
}

/// examples/webhooks (on an in-process node, to reach loopback targets):
/// signatures checked, the sender answered before any target is tried,
/// targets reached after the answer, failures retried with backoff, targets
/// outside the egress list refused by the node, all durable.
#[tokio::test(flavor = "multi_thread")]
async fn webhooks_verify_answer_then_relay_with_retries() {
    let (fast, fast_seen) = upstream(vec![], Duration::from_millis(1500)).await;
    let (flaky, flaky_seen) = upstream(vec![503], Duration::ZERO).await;
    let app = Local::start("webhooks", json!({"limits": {"egress": [fast, flaky]}}));
    let config = json!({"secret": "s3cret", "targets": [format!("http://{fast}/hook"), format!("http://{flaky}/hook"), "http://127.0.0.1:9/no"]});
    assert_eq!(app.send("PUT", "/gh/config", &[], config.to_string()).await.status, 204);

    let body = r#"{"action":"opened","number":7}"#;
    // HMAC-SHA256 (RFC 2104), as GitHub signs.
    use sha2::{Digest, Sha256};
    let key: Vec<u8> = b"s3cret".iter().copied().chain(std::iter::repeat(0)).take(64).collect();
    let pad = |x: u8| key.iter().map(|b| b ^ x).collect::<Vec<u8>>();
    let inner = Sha256::new().chain_update(pad(0x36)).chain_update(body).finalize();
    let mac = Sha256::new().chain_update(pad(0x5c)).chain_update(inner).finalize();
    let signature = format!("sha256={}", mac.iter().map(|b| format!("{b:02x}")).collect::<String>());
    for bad in ["", "sha256=00", "sha1=abc"] {
        assert_eq!(app.send("POST", "/gh/in", &[("x-hub-signature-256", bad)], body).await.status, 401, "{bad}");
    }
    assert_eq!(app.send("POST", "/gh/in", &[("x-hub-signature-256", &signature)], "tampered").await.status, 401);
    let t0 = std::time::Instant::now();
    let signed = [("x-hub-signature-256", signature.as_str()), ("content-type", "application/json")];
    let accepted = app.send("POST", "/gh/in", &signed, body).await;
    assert_eq!((accepted.status, accepted.json()), (202, json!({"id": 1})));
    assert!(t0.elapsed() < Duration::from_millis(1200), "answered before the 1.5 s target: {:?}", t0.elapsed());

    // After the answer: the fast target got it once it was asked; the
    // flaky one failed; one not in the egress list was refused by the node.
    let deliveries = app.send("GET", "/gh/deliveries", &[], "").await.json();
    assert!(t0.elapsed() >= Duration::from_millis(1500), "the object finished relaying before the next request");
    let (headers, got) = fast_seen.lock().unwrap()[0].clone();
    assert_eq!((got.as_str(), &headers["x-relay-delivery"], &headers["content-type"]), (body, &"1".parse::<http::HeaderValue>().unwrap(), &"application/json".parse::<http::HeaderValue>().unwrap()));
    let d = &deliveries[0];
    assert_eq!((&d["done"], &d["tries"]), (&json!([format!("http://{fast}/hook")]), &json!(1)), "{d}");
    assert_eq!(d["pending"].as_array().unwrap().len(), 2);

    // Retried when due, by an external clock: the flaky target now answers.
    assert_eq!(app.send("POST", "/gh/retry", &[], "").await.json()["retried"], 0, "not due yet");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(app.send("POST", "/gh/retry", &[], "").await.json()["retried"], 1);
    let d = app.send("GET", "/gh/deliveries", &[], "").await.json()[0].clone();
    assert_eq!((d["done"].as_array().unwrap().len(), &d["pending"], &d["tries"]), (2, &json!(["http://127.0.0.1:9/no"]), &json!(2)), "{d}");
    assert!(d["error"].as_str().unwrap().contains("127.0.0.1:9/no: ErrorCode::HttpRequestDenied"), "not in the egress list: {d}");
    assert_eq!((flaky_seen.lock().unwrap().len(), fast_seen.lock().unwrap().len()), (2, 1), "each target as often as needed");

    app.evicted().await;
    assert_eq!(app.send("GET", "/gh/deliveries", &[], "").await.json()[0]["tries"], 2);
    let app = app.restart().await;
    assert_eq!(app.send("GET", "/gh/deliveries", &[], "").await.json()[0]["done"].as_array().unwrap().len(), 2);
    assert_eq!(app.send("POST", "/gh/in", &signed, body).await.json()["id"], 2, "the secret survived too");
}

/// A completion API on loopback: a prompt `n N ms M` streams N tokens as
/// Server-Sent Events, one every M ms, then `[DONE]` (`… nodone`: not,
/// `… usage K`: after an event reporting K tokens used); `cut N` streams N
/// and then breaks the connection (a chunked body with no last chunk);
/// `split N` streams N and `[DONE]`, each event in three HTTP chunks 20 ms
/// apart (the first cut inside its `data: ` prefix, the second anywhere
/// after); `fail` is a 500. It records each request's authorization header.
async fn llm() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let keys = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen = keys.clone();
    tokio::spawn(nanovm::server::serve_http1(listener, move |req: http::Request<hyper::body::Incoming>| {
        let seen = seen.clone();
        async move {
            let auth = req.headers().get("authorization").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
            seen.lock().unwrap().push(auth);
            let body = http_body_util::BodyExt::collect(req.into_body()).await.unwrap().to_bytes();
            let prompt = serde_json::from_slice::<Value>(&body).unwrap()["prompt"].as_str().unwrap().to_string();
            let words: Vec<&str> = prompt.split(' ').collect();
            if let ["split", n] = words[..] {
                let n: usize = n.parse().unwrap();
                let events = (0..n).map(|i| format!("data: {{\"token\":\"t{i}\"}}\n\n")).chain(["data: [DONE]\n\n".to_string()]);
                let mut pieces = vec![];
                for (i, e) in events.enumerate() {
                    let a = 1 + i % 6;
                    let b = a + 1 + (i * 7) % (e.len() - a - 1);
                    pieces.extend([e[..a].to_string(), e[a..b].to_string(), e[b..].to_string()]);
                }
                let pieces = futures::stream::iter(pieces).then(|p| async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Ok::<_, nanovm::object::BoxError>(http_body::Frame::data(bytes::Bytes::from(p)))
                });
                let body = http_body_util::BodyExt::boxed_unsync(http_body_util::StreamBody::new(pieces));
                return http::Response::builder().header("content-type", "text/event-stream").body(body).unwrap();
            }
            if let ["cut", n] = words[..] {
                let n: u32 = n.parse().unwrap();
                let tokens = futures::stream::unfold(0, move |i| async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    match i {
                        i if i < n => Some((Ok(http_body::Frame::data(bytes::Bytes::from(format!("data: {{\"token\":\"t{i}\"}}\n\n")))), i + 1)),
                        i if i == n => Some((Err::<http_body::Frame<bytes::Bytes>, nanovm::object::BoxError>("cut".into()), i + 1)),
                        _ => None,
                    }
                });
                let body = http_body_util::BodyExt::boxed_unsync(http_body_util::StreamBody::new(tokens));
                return http::Response::builder().header("content-type", "text/event-stream").body(body).unwrap();
            }
            let ["n", n, "ms", ms, ref then @ ..] = words[..] else { return nanovm::object::error_response(500, "scripted", "fail") };
            let (n, ms): (u32, u64) = (n.parse().unwrap(), ms.parse().unwrap());
            let last = match then {
                ["nodone"] => None,
                ["usage", k] => Some(format!("data: {{\"choices\":[],\"usage\":{{\"completion_tokens\":{k}}}}}\n\ndata: [DONE]\n\n")),
                _ => Some("data: [DONE]\n\n".to_string()),
            };
            let tokens = futures::stream::unfold(0, move |i| {
                let last = last.clone();
                async move {
                    if i > n || (i == n && last.is_none()) {
                        return None;
                    }
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    let event = if i == n { last.unwrap() } else { format!("data: {{\"token\":\"t{i}\"}}\n\n") };
                    Some((Ok::<_, nanovm::object::BoxError>(http_body::Frame::data(bytes::Bytes::from(event))), i + 1))
                }
            });
            let body = http_body_util::BodyExt::boxed_unsync(http_body_util::StreamBody::new(tokens));
            http::Response::builder().header("content-type", "text/event-stream").body(body).unwrap()
        }
    }));
    (addr, keys)
}

/// examples/gateway (on an in-process node, to reach a loopback API): a
/// streamed completion passed through as it is generated; the quota
/// reserved before streaming and refunded after, so a stream cut off by
/// the client, the upstream or post_return_timeout_ms keeps its
/// reservation.
#[tokio::test(flavor = "multi_thread")]
async fn gateway_streams_completions_within_quotas() {
    let (api, keys) = llm().await;
    let patch = json!({
        "limits": {"egress": [api], "post_return_timeout_ms": 1500},
        "config": {"upstream": format!("http://{api}/v1/complete"), "tokens_per_day": "400", "max_tokens": "100"},
        "secrets": {"upstream_key": "test-key"},
    });
    let app = Local::start("gateway", patch);
    let usage = async |app: &Local| app.send("GET", "/ann/usage", &[], "").await.json()["used"].as_i64().unwrap();
    let complete = async |app: &Local, prompt: &str| {
        let req = http::Request::post("http://x/complete").body(bytes::Bytes::from(json!({"prompt": prompt}).to_string())).unwrap();
        app.node.dispatch("ann", req).await
    };

    // Tokens arrive as they are made, and only those used are charged.
    let res = complete(&app, "n 5 ms 100").await;
    assert_eq!((res.status().as_u16(), res.headers()["content-type"].to_str().unwrap()), (200, "text/event-stream"));
    let mut body = res.into_body();
    let mut pieces = vec![];
    while let Some(frame) = http_body_util::BodyExt::frame(&mut body).await {
        if let Ok(data) = frame.unwrap().into_data() {
            pieces.push((std::time::Instant::now(), String::from_utf8_lossy(&data).into_owned()));
        }
    }
    let text: String = pieces.iter().map(|p| p.1.as_str()).collect();
    assert!(text.starts_with("data: {\"token\":\"t0\"}") && text.ends_with("data: [DONE]\n\n") && text.matches("token").count() == 5, "{text}");
    assert!(pieces.last().unwrap().0 - pieces[0].0 >= Duration::from_millis(300), "streamed as generated");
    assert_eq!(usage(&app).await, 5, "100 reserved, 95 given back");
    assert_eq!(keys.lock().unwrap()[0], "Bearer test-key");
    assert_eq!(complete(&app, "fail").await.status(), 502);
    assert_eq!(usage(&app).await, 5, "an upstream failure costs nothing");
    assert_eq!(app.send("POST", "/ann/complete", &[], "nope").await.status, 400);

    // A stream the client drops keeps its reservation.
    let res = complete(&app, "n 50 ms 50").await;
    let mut body = res.into_body();
    http_body_util::BodyExt::frame(&mut body).await.unwrap().unwrap();
    drop(body);
    assert_eq!(usage(&app).await, 105);
    // So does one the upstream breaks off: what came of it is no measure.
    let res = complete(&app, "cut 3").await;
    let text = http_body_util::BodyExt::collect(res.into_body()).await.map(|b| b.to_bytes()).unwrap_or_default();
    let text = String::from_utf8_lossy(&text);
    assert!(text.matches("token").count() == 3 && !text.contains("[DONE]"), "{text}");
    assert_eq!(usage(&app).await, 205, "not refunded as if 3 tokens were all of it");
    // So does one that outlives post_return_timeout_ms: its refund rolls back.
    let res = complete(&app, "n 50 ms 100").await;
    let text = http_body_util::BodyExt::collect(res.into_body()).await.map(|b| b.to_bytes()).unwrap_or_default();
    assert!(!String::from_utf8_lossy(&text).contains("[DONE]"), "cut short");
    assert_eq!(usage(&app).await, 305);
    // 305 + 100 > 400: refused before anything is sent upstream.
    let calls = keys.lock().unwrap().len();
    let over = app.send("POST", "/ann/complete", &[], r#"{"prompt":"n 1 ms 0"}"#).await;
    assert_eq!((over.status, &over.json()["error"]), (429, &json!("quota")));
    assert_eq!(keys.lock().unwrap().len(), calls);
    assert_eq!(app.send("GET", "/bob/usage", &[], "").await.json()["used"], 0, "quotas are per user");

    app.evicted().await;
    assert_eq!(usage(&app).await, 305);
    let app = app.restart().await;
    assert_eq!(usage(&app).await, 305);
}

/// examples/gateway: a token's event split across the upstream's HTTP
/// chunks (as TCP may split it anywhere) is still a token. The gateway reads
/// Server-Sent Events across reads, and refunds only what a stream that
/// verifiably finished (`[DONE]`, and its body whole) didn't use, by the
/// upstream's own count if it gives one.
#[tokio::test(flavor = "multi_thread")]
async fn review_gateway_counts_tokens_split_across_http_chunks() {
    let (api, _) = llm().await;
    let patch = json!({
        "limits": {"egress": [api], "post_return_timeout_ms": 5000},
        "config": {"upstream": format!("http://{api}/v1/complete"), "tokens_per_day": "1000", "max_tokens": "100"},
    });
    let app = Local::start("gateway", patch);
    let usage = async || app.send("GET", "/ann/usage", &[], "").await.json()["used"].as_i64().unwrap();
    let res = app.send("POST", "/ann/complete", &[], json!({"prompt": "split 5"}).to_string()).await;
    assert_eq!(res.status, 200);
    let text = res.text();
    assert!(text.matches("data: {\"token\"").count() == 5 && text.ends_with("data: [DONE]\n\n"), "passed on as sent: {text:?}");
    assert_eq!(usage().await, 5, "5 tokens used, 95 of the 100 reserved given back");
    let complete = async |prompt: &str| app.send("POST", "/ann/complete", &[], json!({"prompt": prompt}).to_string()).await.status;
    assert_eq!(complete("n 3 ms 0 nodone").await, 200);
    assert_eq!(usage().await, 105, "no [DONE], not known to be finished: the reservation stands");
    assert_eq!(complete("n 3 ms 0 usage 7").await, 200);
    assert_eq!(usage().await, 112, "the upstream's count, not the events seen");
}

/// Feeds on loopback: `/rss` and `/atom` serve the documents in `docs`
/// (each with an ETag, and `304` for a matching If-None-Match); every
/// request's path and If-None-Match is recorded.
type Docs = std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, (String, String)>>>;

async fn feed_server(docs: Docs) -> (String, std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>) {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let log = seen.clone();
    tokio::spawn(nanovm::server::serve_http1(listener, move |req: http::Request<hyper::body::Incoming>| {
        let (docs, log) = (docs.clone(), log.clone());
        async move {
            let inm = req.headers().get("if-none-match").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
            log.lock().unwrap().push((req.uri().path().to_string(), inm.clone()));
            let Some((etag, body)) = docs.lock().unwrap().get(req.uri().path()).cloned() else {
                return nanovm::object::error_response(500, "scripted", "no such feed");
            };
            let full = http_body_util::Full::new(bytes::Bytes::from(if inm == etag { String::new() } else { body }));
            let body = http_body_util::BodyExt::boxed_unsync(http_body_util::BodyExt::map_err(full, |e| match e {}));
            http::Response::builder().status(if inm == etag { 304 } else { 200 }).header("etag", etag).body(body).unwrap()
        }
    }));
    (addr, seen)
}

fn rss(items: &[(&str, &str)]) -> String {
    let items: String = items.iter().map(|(t, l)| format!("<item><title>{t}</title><link>{l}</link><guid>{l}#g</guid></item>")).collect();
    format!(r#"<?xml version="1.0"?><rss version="2.0"><channel><title>R</title>{items}</channel></rss>"#)
}

/// examples/feeds (on an in-process node, to reach loopback feeds): RSS and
/// Atom fetched with conditional GETs by one object per feed, each item kept
/// once; readers' timelines gathered from the feeds through calls, one entry
/// per link; all durable, the ETag included.
#[tokio::test(flavor = "multi_thread")]
async fn feeds_fetch_once_and_fan_in_to_readers() {
    let docs: Docs = Default::default();
    docs.lock().unwrap().insert("/rss".into(), ("\"v1\"".into(), rss(&[("Two", "https://x/2"), ("One", "https://x/1")])));
    let atom = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><title>A</title>
        <entry><title>Shared</title><link href="https://x/2"/><id>urn:a:2</id></entry>
        <entry><title>Atom only</title><link rel="alternate" href="https://y/1"/><id>urn:a:1</id></entry></feed>"#;
    docs.lock().unwrap().insert("/atom".into(), ("\"a1\"".into(), atom.into()));
    let (addr, seen) = feed_server(docs.clone()).await;
    let app = Local::start("feeds", json!({"limits": {"egress": [addr]}}));
    let put = async |path: &str, body: Value| app.send("PUT", path, &[], body.to_string()).await;
    assert_eq!(put("/f-rust/", json!({"url": format!("http://{addr}/rss")})).await.status, 204);
    assert_eq!(put("/f-atom/", json!({"url": format!("http://{addr}/atom")})).await.status, 204);
    let refresh = async |feed: &str| app.send("POST", &format!("/{feed}/refresh"), &[], "").await;

    assert_eq!(refresh("f-rust").await.json(), json!({"new": 2}));
    assert_eq!(refresh("f-rust").await.json(), json!({"new": 0, "unchanged": true}), "a 304");
    assert_eq!(seen.lock().unwrap()[1], ("/rss".into(), "\"v1\"".into()), "a conditional GET");
    docs.lock().unwrap().insert("/rss".into(), ("\"v2\"".into(), rss(&[("Three", "https://x/3"), ("Two", "https://x/2"), ("One", "https://x/1")])));
    assert_eq!(refresh("f-rust").await.json(), json!({"new": 1}), "only the new item");
    assert_eq!(refresh("f-atom").await.json(), json!({"new": 2}));
    let items = app.send("GET", "/f-rust/items?after=1", &[], "").await.json();
    let titles: Vec<_> = items.as_array().unwrap().iter().map(|i| i["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["Two", "Three"], "in the feed's order, after 1");

    // Readers gather from the feeds they follow: no fetch, one entry per link.
    let fetches = seen.lock().unwrap().len();
    assert_eq!(put("/ann/follows", json!(["f-rust", "f-atom"])).await.status, 204);
    let timeline = app.send("GET", "/ann/timeline", &[], "").await.json();
    let mut links: Vec<_> = timeline.as_array().unwrap().iter().map(|i| i["link"].as_str().unwrap().to_string()).collect();
    links.sort();
    assert_eq!(links, ["https://x/1", "https://x/2", "https://x/3", "https://y/1"]);
    assert_eq!(app.send("GET", "/ann/timeline", &[], "").await.json(), timeline, "nothing new");
    assert_eq!(put("/bob/follows", json!(["f-rust"])).await.status, 204);
    assert_eq!(app.send("GET", "/bob/timeline", &[], "").await.json().as_array().unwrap().len(), 3);
    assert_eq!(seen.lock().unwrap().len(), fetches, "readers never fetch");
    assert_eq!(put("/ann/follows", json!(["ann"])).await.status, 400);

    // Bad feeds are reported, not stored.
    docs.lock().unwrap().insert("/rss".into(), ("\"v3\"".into(), "<not xml".into()));
    assert_eq!(refresh("f-rust").await.status, 502);
    docs.lock().unwrap().remove("/atom");
    assert_eq!(refresh("f-atom").await.status, 502);

    app.evicted().await;
    let app = app.restart().await;
    assert_eq!(app.send("GET", "/ann/timeline", &[], "").await.json(), timeline);
    docs.lock().unwrap().insert("/rss".into(), ("\"v2\"".into(), String::new()));
    assert_eq!(app.send("POST", "/f-rust/refresh", &[], "").await.json()["unchanged"], true, "the ETag survived");
}

/// A GitHub stand-in on loopback: `POST /token` gives `tok-ann` for the code
/// `good` and the secret `s3cret`; `GET /user` with that token is ann.
async fn github() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(nanovm::server::serve_http1(listener, move |req: http::Request<hyper::body::Incoming>| async move {
        let (path, auth) = (req.uri().path().to_string(), req.headers().get("authorization").cloned());
        let body = http_body_util::BodyExt::collect(req.into_body()).await.unwrap().to_bytes();
        let ask: Value = serde_json::from_slice(&body).unwrap_or_default();
        nanovm::server::ok_json(match path.as_str() {
            "/token" if ask["code"] == "good" && ask["client_secret"] == "s3cret" && ask["client_id"] == "app-1" => json!({"access_token": "tok-ann"}),
            "/token" => json!({"error": "bad_verification_code"}),
            _ if auth.is_some_and(|a| a == "Bearer tok-ann") => json!({"login": "ann", "id": 7, "name": "Ann"}),
            _ => json!({"message": "Bad credentials"}),
        })
    }));
    addr
}

/// examples/oauth (on an in-process node, with GitHub on loopback): the
/// OAuth code flow with a one-time state bound to the browser, a session
/// cookie that only the signed-in user's object accepts, logout, and
/// sessions that survive eviction and restarts.
#[tokio::test(flavor = "multi_thread")]
async fn oauth_signs_in_with_one_time_state_and_scoped_sessions() {
    let gh = github().await;
    let patch = json!({
        "limits": {"egress": [gh]},
        "config": {"authorize_url": "https://github.example/authorize", "token_url": format!("http://{gh}/token"),
                   "user_url": format!("http://{gh}/user"), "client_id": "app-1", "redirect_uri": "http://app/auth/callback"},
        "secrets": {"client_secret": "s3cret"},
    });
    let app = Local::start("oauth", patch);
    // The state, and the browser's nonce cookie it is bound to.
    let login = async |app: &Local| -> (String, String) {
        let to = app.send("GET", "/auth/login", &[], "").await;
        assert_eq!(to.status, 302);
        let location = to.header("location").to_string();
        assert!(location.starts_with("https://github.example/authorize?client_id=app-1&redirect_uri=http://app/auth/callback&state="), "{location}");
        let nonce = to.header("set-cookie");
        assert!(nonce.starts_with("oauth_nonce=") && !nonce.contains("Secure"), "plain http: not Secure, so not __Host-: {nonce}");
        let state = location.split("state=").nth(1).unwrap().split('&').next().unwrap().to_string();
        (state, nonce.split(';').next().unwrap().to_string())
    };
    let (state, nonce) = login(&app).await;
    let back = app.send("GET", &format!("/auth/callback?code=good&state={state}"), &[("cookie", &nonce)], "").await;
    assert_eq!((back.status, back.header("location")), (302, "/u-ann/me"));
    let cookie = back.header("set-cookie").split(';').next().unwrap().to_string();
    assert!(cookie.starts_with("session=ann.") && back.header("set-cookie").contains("HttpOnly"), "{cookie}");
    let me = [("cookie", cookie.as_str())];
    assert_eq!(app.send("GET", "/u-ann/me", &me, "").await.json(), json!({"login": "ann", "id": 7, "name": "Ann"}));
    assert_eq!(app.send("GET", "/u-ann/me", &[], "").await.status, 401);
    assert_eq!(app.send("GET", "/u-bob/me", &me, "").await.status, 401, "ann's cookie is only ann's");
    let forged = format!("session=ann.{}", "0".repeat(64));
    assert_eq!(app.send("GET", "/u-ann/me", &[("cookie", &forged)], "").await.status, 401, "a made-up session");

    // A state is used once, and only a state we made counts.
    let with_nonce = [("cookie", nonce.as_str())];
    assert_eq!(app.send("GET", &format!("/auth/callback?code=good&state={state}"), &with_nonce, "").await.status, 400);
    assert_eq!(app.send("GET", "/auth/callback?code=good&state=forged", &with_nonce, "").await.status, 400);
    let (state, nonce) = login(&app).await;
    assert_eq!(app.send("GET", &format!("/auth/callback?code=bad&state={state}"), &[("cookie", &nonce)], "").await.status, 401);

    // Sessions outlive instances and the node; logout ends them everywhere.
    app.evicted().await;
    let app = app.restart().await;
    assert_eq!(app.send("GET", "/u-ann/me", &me, "").await.status, 200);
    assert_eq!(app.send("POST", "/u-ann/logout", &me, "").await.status, 204);
    assert_eq!(app.send("GET", "/u-ann/me", &me, "").await.status, 401);
}

/// examples/oauth: a sign-in's `state` works only in the browser that asked
/// for it. Without that, someone who starts a sign-in and gets another
/// browser to open the callback link (their code, their state) signs that
/// browser in as themselves (login CSRF). `/auth/login` sets a random nonce
/// in a `__Host-` cookie (HttpOnly, Secure, SameSite=Lax, Path=/), bound by
/// its hash to the state; the callback needs both, and the state is spent
/// by any attempt.
#[tokio::test(flavor = "multi_thread")]
async fn review_oauth_state_must_be_bound_to_the_starting_browser() {
    let gh = github().await;
    let patch = json!({
        "limits": {"egress": [gh]},
        "config": {"authorize_url": "https://github.example/authorize", "token_url": format!("http://{gh}/token"),
                   "user_url": format!("http://{gh}/user"), "client_id": "app-1", "redirect_uri": "https://app.example/auth/callback"},
        "secrets": {"client_secret": "s3cret"},
    });
    let app = Local::start("oauth", patch);
    // A browser starts a sign-in; `cookies` is what it already holds.
    let login = async |cookies: &str| -> (String, String) {
        let to = app.send("GET", "/auth/login", &[("cookie", cookies)], "").await;
        assert_eq!(to.status, 302);
        let state = to.header("location").split("state=").nth(1).unwrap().split('&').next().unwrap().to_string();
        (state, to.header("set-cookie").to_string())
    };
    let callback = async |state: &str, cookies: &str| {
        let back = app.send("GET", &format!("/auth/callback?code=good&state={state}"), &[("cookie", cookies)], "").await;
        let session = back.headers.get_all("set-cookie").iter().any(|c| c.to_str().unwrap().starts_with("session="));
        (back.status, session)
    };

    // The link opened in a browser that didn't start it signs no one in.
    let (state, set) = login("").await;
    assert_eq!(callback(&state, "").await, (400, false), "no nonce: whoever opens the link is signed in");
    let attrs: Vec<&str> = set.split(';').map(str::trim).collect();
    let nonce = attrs[0];
    assert!(nonce.starts_with("__Host-") && nonce.split_once('=').unwrap().1.len() >= 32, "a random nonce in a __Host- cookie: {set}");
    for attr in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/"] {
        assert!(attrs.contains(&attr), "{attr}: {set}");
    }
    assert!(!set.to_ascii_lowercase().contains("domain="), "{set}");
    let (state, _) = login(nonce).await;
    let (_, other) = login("").await;
    let other = other.split(';').next().unwrap().to_string();
    assert_ne!(other, nonce);
    assert_eq!(callback(&state, &other).await, (400, false), "another browser's nonce");
    assert_eq!(callback(&state, nonce).await, (400, false), "the state is spent by any attempt");

    // The browser that started it signs in, even with two sign-ins open.
    let (first, _) = login(nonce).await;
    let (second, again) = login(nonce).await;
    assert_eq!(again.split(';').next().unwrap(), nonce, "a browser keeps its nonce");
    assert_eq!(callback(&second, nonce).await, (302, true));
    assert_eq!(callback(&first, nonce).await, (302, true));
}

/// An example as an operator runs it: published to a registry on loopback
/// without its secrets, and served by a supervised node (dev mode, so no
/// signatures and loopback egress) whose secrets directory gives them, in a
/// jailed worker (as a uid of its own when the tests run as root).
struct Served {
    _sup: std::sync::Arc<nanovm::supervisor::Supervisor>,
    addr: std::net::SocketAddr,
    app: String,
    host: String,
    http: reqwest::Client,
}

impl Served {
    async fn start(name: &str, patch: Value, secrets: Value) -> Served {
        use std::os::unix::fs::PermissionsExt;
        static REGISTRY: std::sync::OnceLock<container_registry::test_support::RunningRegistry> = std::sync::OnceLock::new();
        let registry = REGISTRY.get_or_init(|| container_registry::ContainerRegistry::builder().build_for_testing().run_in_background()).bound_addr();
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
        let (data, dir) = (root.join("examples-served").join(name), root.join("examples-secrets").join(name));
        for d in [&data, &dir] {
            let _ = std::fs::remove_dir_all(d);
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir_all(root.join("cwasm-cache")).unwrap();
        std::os::unix::fs::symlink(root.join("cwasm-cache"), data.join("cache")).unwrap();

        // The package: what `nanovm publish` would push, which holds no secret.
        let manifest: Value = serde_json::from_slice(&std::fs::read(repo().join(format!("examples/{name}/nanovm.json"))).unwrap()).unwrap();
        let manifest = patched(manifest, &patch);
        nanovm::secrets::check_package(&serde_json::from_value(manifest.clone()).unwrap()).unwrap();
        for value in secrets.as_object().unwrap().values() {
            assert!(!manifest.to_string().contains(value.as_str().unwrap()), "the package holds a secret");
        }
        let wasm = std::fs::read(repo().join(format!("examples/target/{name}.wasm"))).unwrap();
        let r = format!("{registry}/examples/{name}:1.0.0").parse().unwrap();
        let oci = nanovm::registry::Oci::new(true).unwrap();
        let digest = oci.push_app(&r, None, manifest.to_string().into_bytes(), wasm, &"1.0.0".parse().unwrap()).await.unwrap();
        let app = format!("{registry}/examples/{name}@{digest}");

        // The operator's part: the app's secrets, named as the node names it.
        let host = format!("{name}.example");
        let file = dir.join(format!("{}.json", nanovm::registry::app_name(&host, &app.parse().unwrap())));
        std::fs::write(file, secrets.to_string()).unwrap();
        let cfg = nanovm::config::NodeConfig {
            data_dir: data,
            secrets: Some(dir),
            max_live_objects: 16,
            memory_reservation_mb: 64,
            dev: true,
            ..Default::default()
        };
        let sup = nanovm::supervisor::Supervisor::new(cfg, PathBuf::from(env!("CARGO_BIN_EXE_nanovm")), 1, None).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let s = sup.clone();
        tokio::spawn(nanovm::server::serve_http1(listener, move |req| s.clone().handle(req)));
        let http = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        Served { _sup: sup, addr, app, host, http }
    }

    /// A request as the proxy sends it on: naming the app and its host.
    async fn send(&self, method: &str, path: &str, headers: &[(&str, &str)], body: impl Into<reqwest::Body>) -> Reply {
        let mut req = self.http.request(Method::from_bytes(method.as_bytes()).unwrap(), format!("http://{}{path}", self.addr)).body(body)
            .header("nanovm-app", &self.app).header("nanovm-trust", "dev dev").header("nanovm-host", &self.host);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let res = req.send().await.unwrap_or_else(|e| panic!("{method} {path}: {e:?}"));
        let (status, headers) = (res.status().as_u16(), res.headers().clone());
        Reply { status, headers, body: res.bytes().await.unwrap().to_vec() }
    }
}

/// examples/oauth and examples/gateway as an operator runs them: their
/// packages hold no key (`nanovm publish` would take them), and the node's
/// secrets directory gives each its own, which its jailed worker reads with
/// its config. GitHub's stand-in signs in only with the client secret, and
/// the API's stand-in records the key it is sent.
#[tokio::test(flavor = "multi_thread")]
async fn oauth_and_gateway_get_their_keys_from_the_node() {
    let gh = github().await;
    let oauth = Served::start("oauth", json!({
        "limits": {"egress": [gh]},
        "config": {"authorize_url": "https://github.example/authorize", "token_url": format!("http://{gh}/token"),
                   "user_url": format!("http://{gh}/user"), "client_id": "app-1", "redirect_uri": "http://app/auth/callback"},
    }), json!({"client_secret": "s3cret"})).await;
    let to = oauth.send("GET", "/auth/login", &[], "").await;
    assert_eq!(to.status, 302, "{to:?}");
    let state = to.header("location").split("state=").nth(1).unwrap().split('&').next().unwrap().to_string();
    let nonce = to.header("set-cookie").split(';').next().unwrap().to_string();
    let back = oauth.send("GET", &format!("/auth/callback?code=good&state={state}"), &[("cookie", &nonce)], "").await;
    assert_eq!((back.status, back.header("location")), (302, "/u-ann/me"), "{back:?}");
    let session = back.header("set-cookie").split(';').next().unwrap().to_string();
    let me = oauth.send("GET", "/u-ann/me", &[("cookie", &session)], "").await;
    assert_eq!(me.json(), json!({"login": "ann", "id": 7, "name": "Ann"}));

    let (api, keys) = llm().await;
    let gateway = Served::start("gateway", json!({
        "limits": {"egress": [api]},
        "config": {"upstream": format!("http://{api}/v1/complete"), "tokens_per_day": "400", "max_tokens": "100"},
    }), json!({"upstream_key": "test-key"})).await;
    let res = gateway.send("POST", "/ann/complete", &[], json!({"prompt": "n 2 ms 0"}).to_string()).await;
    assert!(res.status == 200 && res.text().ends_with("data: [DONE]\n\n"), "{res:?}");
    assert_eq!(keys.lock().unwrap().as_slice(), ["Bearer test-key"]);
}
