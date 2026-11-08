//! The example apps in `examples/`, each under `nanovm run` as a developer
//! runs it: what it does, that its state survives eviction (objects idle out
//! after 300 ms here) and a restart of the node, and that it refuses bad
//! input. Build them first: `tools/build-app.sh examples/<name> -o
//! examples/target/<name>.wasm` (CI does it for every example).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::Method;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// An example app under `nanovm run`, with its own data directory.
struct App {
    args: Vec<OsString>,
    addr: std::net::SocketAddr,
    lines: tokio::sync::mpsc::UnboundedReceiver<String>,
    _child: tokio::process::Child,
    http: reqwest::Client,
}

impl App {
    /// Serves `examples/target/{name}.wasm` with `examples/{name}/nanovm.json`
    /// (if any) and `limits` changed, objects idling out after 300 ms.
    async fn start(name: &str, limits: Value) -> App {
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
        for (k, v) in limits.as_object().unwrap() {
            manifest["limits"][k] = v.clone();
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
        App { args, addr, lines, _child: child, http }
    }

    /// Stops the node and starts it again on the same data.
    async fn restart(self) -> App {
        let args = self.args.clone();
        drop(self);
        App::spawn(args).await
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

    async fn send(&self, method: &str, path: &str, headers: &[(&str, &str)], body: impl Into<reqwest::Body>) -> Reply {
        let mut req = self.http.request(Method::from_bytes(method.as_bytes()).unwrap(), self.url(path)).body(body);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let res = req.send().await.unwrap_or_else(|e| panic!("{method} {path}: {e}"));
        let (status, headers) = (res.status().as_u16(), res.headers().clone());
        let body = res.bytes().await.unwrap().to_vec();
        Reply { status, headers, body }
    }

    async fn get(&self, path: &str) -> Reply {
        self.send("GET", path, &[], "").await
    }

    async fn post(&self, path: &str, body: impl Into<reqwest::Body>) -> Reply {
        self.send("POST", path, &[], body).await
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
