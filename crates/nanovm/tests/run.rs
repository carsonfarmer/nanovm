//! `nanovm run`, the binary itself, as a developer uses it: one app on
//! loopback with WebSockets, a console line per request, and a restart when
//! the component is rebuilt.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use tokio::io::AsyncBufReadExt;
use tokio_tungstenite::tungstenite::{Error, Message, protocol::frame::coding::CloseCode};

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// A built component; the test guests are composed with the key-value provider.
fn wasm(path: &str) -> PathBuf {
    let p = repo().join(path);
    assert!(p.exists(), "{} is missing: run scripts/build-guests.sh (and tools/build-app.sh for examples)", p.display());
    p
}

fn guest(name: &str) -> PathBuf {
    wasm(&format!("guests/target/wasm32-wasip2/release/{name}.wasm"))
}

/// An empty data directory for `test`, sharing the tests' compile cache.
fn data_dir(test: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let dir = root.join("run").join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(root.join("cwasm-cache")).unwrap();
    std::os::unix::fs::symlink(root.join("cwasm-cache"), dir.join("cache")).unwrap();
    dir
}

/// `nanovm run`, killed when dropped, and the lines it prints.
struct Running {
    _child: tokio::process::Child,
    lines: tokio::sync::mpsc::UnboundedReceiver<String>,
}

fn start(args: &[&OsStr]) -> Running {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_nanovm"))
        .arg("run")
        .args(args)
        .args(["--listen", "127.0.0.1:0"])
        .env_remove("RUST_BACKTRACE")
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stderr = tokio::io::BufReader::new(child.stderr.take().unwrap()).lines();
    let (tx, lines) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(Some(line)) = stderr.next_line().await {
            eprintln!("nanovm run: {line}");
            let _ = tx.send(line);
        }
    });
    Running { _child: child, lines }
}

impl Running {
    /// The next line that contains `text`.
    async fn line(&mut self, text: &str) -> String {
        loop {
            let line = tokio::time::timeout(Duration::from_secs(60), self.lines.recv()).await;
            let line = line.unwrap_or_else(|_| panic!("no line with {text:?} in time")).expect("nanovm run exited");
            if line.contains(text) {
                return line;
            }
        }
    }

    /// The address it says it serves on, next.
    async fn addr(&mut self) -> std::net::SocketAddr {
        let line = self.line(" on http://").await;
        line.split("http://").nth(1).unwrap().split('/').next().unwrap().parse().unwrap()
    }
}

/// Sends one raw HTTP/1.1 request: the status and body.
async fn raw(addr: std::net::SocketAddr, head: &str, body: &[u8]) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(head.as_bytes()).await.unwrap();
    let _ = s.write_all(body).await;
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out).await;
    let text = String::from_utf8_lossy(&out).to_string();
    let status = text.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    (status, text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default())
}

async fn get(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
    raw(addr, &format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"), b"").await
}

async fn post(addr: std::net::SocketAddr, path: &str, body: &str) -> (u16, String) {
    let head = format!("POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    raw(addr, &head, body.as_bytes()).await
}

/// The socket, or the refusal's status.
async fn open(addr: std::net::SocketAddr, path: &str) -> Result<Ws, u16> {
    match tokio_tungstenite::connect_async(format!("ws://{addr}{path}")).await {
        Ok((ws, _)) => Ok(ws),
        Err(Error::Http(res)) => Err(res.status().as_u16()),
        Err(e) => panic!("{path}: {e}"),
    }
}

/// The next data message or close frame.
async fn next(ws: &mut Ws) -> Message {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.expect("a message in time");
        match m.expect("the socket is open").expect("a frame") {
            Message::Ping(_) | Message::Pong(_) => continue,
            m => return m,
        }
    }
}

async fn say(ws: &mut Ws, text: &str) {
    ws.send(Message::text(text)).await.unwrap();
}

/// Unsigned apps run outside the jail, so only on loopback.
#[tokio::test(flavor = "multi_thread")]
async fn nanovm_run_listens_on_loopback_only() {
    let mut run = tokio::process::Command::new(env!("CARGO_BIN_EXE_nanovm"));
    run.args(["run".as_ref(), guest("counter").as_os_str(), "--listen".as_ref(), "0.0.0.0:0".as_ref()]);
    let out = tokio::time::timeout(Duration::from_secs(30), run.kill_on_drop(true).output()).await;
    let out = out.expect("refused, not served").unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && stderr.contains("only on loopback"), "{stderr}");
}

/// A component that imports `spin:key-value` without the provider composed
/// in is refused with what to do about it, not served.
#[tokio::test(flavor = "multi_thread")]
async fn nanovm_run_says_how_to_compose_key_value() {
    let bare = repo().join("target/guests/wasm32-wasip2/release/counter.wasm");
    assert!(bare.exists(), "{} is missing: run scripts/build-guests.sh", bare.display());
    let data = data_dir("compose_key_value");
    let mut run = tokio::process::Command::new(env!("CARGO_BIN_EXE_nanovm"));
    run.args(["run".as_ref(), bare.as_os_str(), "--data".as_ref(), data.as_os_str(), "--listen".as_ref(), "127.0.0.1:0".as_ref()]);
    let out = tokio::time::timeout(Duration::from_secs(60), run.kill_on_drop(true).output()).await;
    let stderr = String::from_utf8_lossy(&out.expect("refused, not served").unwrap().stderr).into_owned();
    assert!(stderr.contains("spin:key-value") && stderr.contains("tools/build-app.sh"), "{stderr}");
}

/// One command serves an app as a node and its proxy would: objects with
/// their own volumes, the manifest's config, WebSockets whose publishes
/// arrive only once their request commits, and a line per request.
#[tokio::test(flavor = "multi_thread")]
async fn nanovm_run_serves_requests_sockets_and_publishes() {
    let data = data_dir("serves");
    let manifest = repo().join("guests/counter/nanovm.json");
    let args = [guest("counter").into_os_string(), "--manifest".into(), manifest.into(), "--data".into(), data.into()];
    let mut run = start(&args.iter().map(|a| a.as_os_str()).collect::<Vec<_>>());
    let addr = run.addr().await;
    assert_eq!(post(addr, "/alice/incr", "").await, (200, r#"{"count":1,"instance_hits":1}"#.into()));
    assert!(get(addr, "/bob/").await.1.contains(r#""count":0"#), "objects have their own volumes");
    assert_eq!(get(addr, "/alice/config/greeting").await, (200, "from the registry".into()));
    assert_eq!(post(addr, "/alice/kv/k", "v").await.0, 204);
    assert_eq!(get(addr, "/alice/kv/k").await, (200, "v".into()), "the composed key-value provider works");
    let line = run.line("POST /alice/incr").await;
    assert!(line.contains("→ 200 in") && line.ends_with("cold start"), "{line}");
    let line = run.line("GET /alice/config/greeting").await;
    assert!(line.contains("→ 200 in") && !line.contains("cold"), "{line}");

    let (mut a, mut b) = (open(addr, "/room/ws").await.unwrap(), open(addr, "/room/ws").await.unwrap());
    say(&mut a, "hello").await;
    assert_eq!(next(&mut a).await, Message::text("echo hello"));
    say(&mut a, "pub hi").await;
    assert_eq!((next(&mut a).await, next(&mut b).await), (Message::text("hi"), Message::text("hi")));
    assert_eq!(post(addr, "/room/publish?msg=yo", "").await.0, 200);
    assert_eq!((next(&mut a).await, next(&mut b).await), (Message::text("yo"), Message::text("yo")));
    // A request that fails publishes nothing, and its socket is closed.
    say(&mut b, "pubpanic lost").await;
    let Message::Close(Some(f)) = next(&mut b).await else { panic!("no close frame") };
    assert_eq!(f.code, CloseCode::Error);
    say(&mut a, "pub kept").await;
    assert_eq!(next(&mut a).await, Message::text("kept"));
    // Only the proxy, here `nanovm run`, sends socket events.
    let forged = "POST /room/ws HTTP/1.1\r\nHost: x\r\nContent-Type: application/websocket-events\r\n\
        Connection-Id: 1\r\nContent-Length: 12\r\nConnection: close\r\n\r\nDISCONNECT\r\n";
    assert_eq!(raw(addr, forged, b"").await.0, 400);
    drop(b);
    a.close(None).await.unwrap();
    let t0 = Instant::now();
    while get(addr, "/room/kv/ws-ends").await != (200, "2".into()) {
        assert!(t0.elapsed() < Duration::from_secs(10), "the object never heard both sockets end");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Rebuilding the component restarts `nanovm run` on the new one, with the
/// same data.
#[tokio::test(flavor = "multi_thread")]
async fn nanovm_run_restarts_when_the_component_changes() {
    let data = data_dir("restarts");
    let app = data.join("app.wasm");
    std::fs::copy(guest("counter"), &app).unwrap();
    let mut run = start(&[app.as_os_str(), "--data".as_ref(), data.as_os_str()]);
    let addr = run.addr().await;
    assert_eq!(post(addr, "/x/incr", "").await.0, 200);
    std::fs::copy(guest("hostile"), &app).unwrap();
    run.line("changed; restarting").await;
    let addr = run.addr().await;
    assert_eq!(get(addr, "/x/").await, (200, r#"{"object":"x","hits":1}"#.into()));
    assert_eq!(std::fs::read_to_string(data.join("objects/app/x/count")).unwrap(), "1");
}

/// The quickstart (README, sdk/rust/README.md): examples/hello, built with
/// tools/build-app.sh, counts each object's visits in its composed
/// key-value store, which survives a restart.
#[tokio::test(flavor = "multi_thread")]
async fn the_quickstart_app_counts_visits() {
    let data = data_dir("quickstart");
    let args = [wasm("examples/target/hello.wasm").into_os_string(), "--data".into(), data.into()];
    let args: Vec<&OsStr> = args.iter().map(|a| a.as_os_str()).collect();
    let mut run = start(&args);
    let addr = run.addr().await;
    assert_eq!(get(addr, "/alice/").await, (200, "alice has had 1 visits\n".into()));
    assert_eq!(get(addr, "/alice/").await, (200, "alice has had 2 visits\n".into()));
    assert_eq!(get(addr, "/bob/").await, (200, "bob has had 1 visits\n".into()));
    let line = run.line("GET /bob/").await;
    assert!(line.contains("→ 200 in") && line.ends_with("cold start"), "{line}");
    drop(run);
    let mut run = start(&args);
    let addr = run.addr().await;
    assert_eq!(get(addr, "/alice/").await, (200, "alice has had 3 visits\n".into()), "durable across restarts");
}

/// The example chat: rooms are objects whose history survives eviction and
/// restarts, members are WebSockets, and the lobby indexes rooms through
/// calls between objects.
#[tokio::test(flavor = "multi_thread")]
async fn the_chat_example_is_live_and_durable() {
    let data = data_dir("chat_example");
    // The example's limits, with objects evicted after 300 ms idle.
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(repo().join("examples/chat/nanovm.json")).unwrap()).unwrap();
    manifest["limits"]["idle_timeout_ms"] = 300.into();
    std::fs::write(data.join("nanovm.json"), manifest.to_string()).unwrap();
    let args = [
        wasm("examples/target/chat.wasm").into_os_string(),
        "--manifest".into(),
        data.join("nanovm.json").into(),
        "--data".into(),
        data.clone().into(),
    ];
    let args: Vec<&OsStr> = args.iter().map(|a| a.as_os_str()).collect();
    let message = |m: Message| -> serde_json::Value { serde_json::from_str(m.to_text().unwrap()).unwrap() };
    let texts = |v: serde_json::Value| -> Vec<String> {
        v.as_array().unwrap().iter().map(|m| m["text"].as_str().unwrap().into()).collect()
    };

    let mut run = start(&args);
    let addr = run.addr().await;
    let (status, page) = get(addr, "/lobby/").await;
    assert!(status == 200 && page.contains("<title>nanovm chat</title>"), "{status} {page}");

    let mut ann = open(addr, "/general/ws?name=ann").await.unwrap();
    let mut bob = open(addr, "/general/ws?name=bob").await.unwrap();
    say(&mut ann, "hello").await;
    for ws in [&mut ann, &mut bob] {
        let m = message(next(ws).await);
        assert_eq!((m["name"].as_str(), m["text"].as_str()), (Some("ann"), Some("hello")), "{m}");
    }
    assert_eq!(post(addr, "/general/messages?name=carl", "").await.0, 400);
    assert_eq!(post(addr, "/general/messages?name=carl", "over http").await.0, 200);
    for ws in [&mut ann, &mut bob] {
        assert_eq!(message(next(ws).await)["text"], "over http");
    }
    // The lobby heard of each message through a call between objects.
    let (status, rooms) = get(addr, "/lobby/rooms").await;
    let rooms: serde_json::Value = serde_json::from_str(&rooms).unwrap();
    assert_eq!(status, 200);
    assert_eq!((rooms[0]["room"].as_str(), rooms[0]["messages"].as_u64()), (Some("general"), Some(2)), "{rooms}");
    assert_eq!(rooms[0]["last"]["name"], "carl");
    let mut lobby = open(addr, "/lobby/ws").await.unwrap();
    assert_eq!(message(next(&mut lobby).await)["messages"], 2);
    say(&mut bob, "and live").await;
    assert_eq!(message(next(&mut lobby).await)["messages"], 3);
    for ws in [&mut ann, &mut bob] {
        assert_eq!(message(next(ws).await)["text"], "and live");
    }

    // Idle rooms are evicted. Their history is in their volume, and their
    // sockets outlive them.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let (status, history) = get(addr, "/general/messages").await;
    let history = texts(serde_json::from_str(&history).unwrap());
    assert_eq!((status, history), (200, vec!["hello".into(), "over http".into(), "and live".into()]));
    let line = run.line("GET /general/messages").await;
    assert!(line.ends_with("cold start"), "evicted, so started again: {line}");
    say(&mut ann, "after eviction").await;
    for ws in [&mut ann, &mut bob] {
        assert_eq!(message(next(ws).await)["text"], "after eviction");
    }

    // A restart keeps everything, and a new member gets the history.
    drop(run);
    let mut run = start(&args);
    let addr = run.addr().await;
    let mut dee = open(addr, "/general/ws?name=dee").await.unwrap();
    let mut seen = vec![];
    for _ in 0..4 {
        seen.push(message(next(&mut dee).await)["text"].as_str().unwrap().to_string());
    }
    assert_eq!(seen, ["hello", "over http", "and live", "after eviction"]);
    let rooms: serde_json::Value = serde_json::from_str(&get(addr, "/lobby/rooms").await.1).unwrap();
    assert_eq!((rooms[0]["messages"].as_u64(), rooms[0]["last"]["text"].as_str()), (Some(4), Some("after eviction")));
    // Bad input is refused, not stored.
    assert_eq!(post(addr, "/lobby/rooms/general", "not json").await.0, 400);
    for bad in ["%3Cb%3E", "_x", ".x"] {
        let entry = r#"{"messages":1,"last":{"name":"a","text":"b","at":1}}"#;
        assert_eq!(post(addr, &format!("/lobby/rooms/{bad}"), entry).await.0, 400, "{bad}");
    }
    say(&mut dee, &"x".repeat(2001)).await;
    assert!(message(next(&mut dee).await)["error"].is_string());
    assert_eq!(texts(serde_json::from_str(&get(addr, "/general/messages").await.1).unwrap()).len(), 4);
}

/// A JavaScript app (js/tests/app.js) under `nanovm run`: `env.kv` on the
/// composed provider, and `env.publish` reaching its object's sockets, which
/// needs a node that delivers publishes in-process.
#[tokio::test(flavor = "multi_thread")]
async fn javascript_apps_run_and_publish() {
    let data = data_dir("javascript");
    let mut run = start(&[wasm("js/target/test-app.wasm").as_os_str(), "--data".as_ref(), data.as_os_str()]);
    let addr = run.addr().await;
    assert_eq!(post(addr, "/a/incr", "").await, (200, r#"{"n":1,"hits":1}"#.into()));
    assert_eq!(post(addr, "/a/incr", "").await, (200, r#"{"n":2,"hits":2}"#.into()));
    let (mut a, mut b) = (open(addr, "/room/ws").await.unwrap(), open(addr, "/room/ws").await.unwrap());
    say(&mut a, "hello").await;
    assert_eq!(next(&mut a).await.to_text().unwrap().split(" from ").next(), Some("echo hello"));
    say(&mut a, "pub hi").await;
    assert_eq!((next(&mut a).await, next(&mut b).await), (Message::text("hi"), Message::text("hi")));
}
