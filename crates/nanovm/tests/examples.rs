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
