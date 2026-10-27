//! A deliberately hostile component. Every endpoint tries to break out of, or
//! exhaust, one of the host's limits. The host is expected to contain each one.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};

use util::wasip3;
use util::{js, json, read_req, text, FdFlags, OFlags, Req};
use wasip3::filesystem::types::PathFlags;
use wasip3::http::types::{ErrorCode, Fields, Method, Request, Response, Scheme};
use wasip3::wit_future;

wasip3::http::service::export!(Hostile);

struct Hostile;

static HITS: AtomicU64 = AtomicU64::new(0);
static ACTIVE: AtomicU64 = AtomicU64::new(0);
static MAX_ACTIVE: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static HOARD: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
    static LEAKED: RefCell<Vec<Fields>> = const { RefCell::new(Vec::new()) };
}

impl wasip3::exports::http::handler::Guest for Hostile {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let req = read_req(request).await;
        HITS.fetch_add(1, Ordering::Relaxed);
        let now = ACTIVE.fetch_add(1, Ordering::SeqCst) + 1;
        MAX_ACTIVE.fetch_max(now, Ordering::SeqCst);
        let res = route(req).await;
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
        res
    }
}

fn ms(n: u64) -> u64 {
    n * 1_000_000
}

async fn route(req: Req) -> Result<Response, ErrorCode> {
    match req.path.as_str() {
        "/" => json(
            200,
            format!(
                "{{\"object\":{},\"hits\":{}}}",
                js(&util::env("NANOVM_OBJECT_ID")),
                HITS.load(Ordering::Relaxed)
            ),
        ),
        "/echo" => util::respond(200, "application/octet-stream", req.body),

        // Grow linear memory by `mb` MiB, touching every page, and keep it.
        "/alloc" => {
            let mb = req.q_u64("mb", 1024);
            for _ in 0..mb {
                let mut v = vec![0u8; 1 << 20];
                for i in (0..v.len()).step_by(4096) {
                    v[i] = 1;
                }
                HOARD.with(|h| h.borrow_mut().push(v));
            }
            let held: usize = HOARD.with(|h| h.borrow().len());
            json(200, format!("{{\"held_mb\":{held}}}"))
        }

        // Burn CPU forever.
        "/spin" => {
            let mut x: u64 = 0;
            loop {
                x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
            }
        }

        // Sleep (async, no CPU) for `ms` milliseconds.
        "/sleep" => {
            let n = req.q_u64("ms", 60_000);
            wasip3::clocks::monotonic_clock::wait_for(ms(n)).await;
            json(200, format!("{{\"slept_ms\":{n}}}"))
        }

        "/panic" => panic!("hostile guest panicked on purpose"),

        // Concurrency probe: hold the request for `ms`, report max overlap seen.
        "/concurrency" => {
            wasip3::clocks::monotonic_clock::wait_for(ms(req.q_u64("ms", 20))).await;
            json(200, format!("{{\"max_active\":{}}}", MAX_ACTIVE.load(Ordering::SeqCst)))
        }

        // Write 1 MiB chunks to `/{mount}/fill.bin` until `mb` or an error.
        "/fill" => {
            let mount = format!("/{}", req.q("mount").unwrap_or("tmp"));
            let mb = req.q_u64("mb", 1 << 20);
            let name = req.q("name").unwrap_or("fill.bin").to_string();
            let fd = match util::open(&mount, &name, OFlags::CREATE, FdFlags::READ | FdFlags::WRITE).await {
                Ok(fd) => fd,
                Err(e) => return fs_fail(0, &e),
            };
            let mut written = 0u64;
            for i in 0..mb {
                if let Err(e) = util::write_all(&fd, vec![0xAB; 1 << 20], i << 20).await {
                    let st = fd.stat().await.map(|s| s.size).unwrap_or(written);
                    return fs_fail(st, &e);
                }
                written += 1 << 20;
            }
            json(200, format!("{{\"written\":{written}}}"))
        }

        // One file of `mb` MiB in a single write.
        "/bigfile" => {
            let mount = format!("/{}", req.q("mount").unwrap_or("tmp"));
            let mb = req.q_u64("mb", 64);
            match util::write_file(&mount, "big.bin", vec![7u8; (mb as usize) << 20]).await {
                Ok(()) => json(200, format!("{{\"written\":{}}}", mb << 20)),
                Err(e) => fs_fail(0, &e),
            }
        }

        // Create `n` small files until the file-count limit.
        "/files" => {
            let mount = format!("/{}", req.q("mount").unwrap_or("tmp"));
            let n = req.q_u64("n", 1_000_000);
            for i in 0..n {
                if let Err(e) = util::write_file(&mount, &format!("f{i}"), b"x".to_vec()).await {
                    return json(507, format!("{{\"created\":{i},\"error\":{}}}", js(&util::fs_err(&e))));
                }
            }
            json(200, format!("{{\"created\":{n}}}"))
        }

        // Delete files created by /fill, /bigfile and /files.
        "/clean" => {
            let mount = format!("/{}", req.q("mount").unwrap_or("tmp"));
            let dir = util::mount(&mount).unwrap();
            let mut removed = 0;
            for name in ["fill.bin", "big.bin"] {
                if dir.unlink_file_at(name.to_string()).await.is_ok() {
                    removed += 1;
                }
            }
            for i in 0..req.q_u64("n", 0) {
                if dir.unlink_file_at(format!("f{i}")).await.is_ok() {
                    removed += 1;
                }
            }
            json(200, format!("{{\"removed\":{removed}}}"))
        }

        "/write-app" => match util::write_file("/app", "pwned.txt", b"pwned".to_vec()).await {
            Ok(()) => json(200, "{\"wrote\":true}"),
            Err(e) => fs_fail(0, &e),
        },

        // Try to read outside the sandbox in every way we can think of.
        "/escape" => {
            let mut results = Vec::new();
            for (mount, path) in [
                ("/data", "../../../../../../etc/passwd"),
                ("/data", "/etc/passwd"),
                ("/tmp", "../"),
                ("/app", "../../data"),
            ] {
                let r = match util::read_file(mount, path).await {
                    Ok(b) => format!("LEAKED {} bytes", b.len()),
                    Err(e) => util::fs_err(&e),
                };
                results.push(format!("{{\"mount\":{},\"path\":{},\"result\":{}}}", js(mount), js(path), js(&r)));
            }
            // Symlink pointing outside, then follow it.
            let data = util::mount("/data").unwrap();
            let sl = data.symlink_at("/etc/passwd".to_string(), "evil-link".to_string()).await;
            let follow = match data
                .open_at(PathFlags::SYMLINK_FOLLOW, "evil-link".to_string(), OFlags::empty(), FdFlags::READ)
                .await
            {
                Ok(_) => "OPENED".to_string(),
                Err(e) => util::fs_err(&e),
            };
            let _ = data.unlink_file_at("evil-link".to_string()).await;
            results.push(format!(
                "{{\"symlink\":{},\"follow\":{}}}",
                js(&match sl { Ok(()) => "created".into(), Err(e) => util::fs_err(&e) }),
                js(&follow)
            ));
            let leaked = results.iter().any(|r| r.contains("LEAKED") || r.contains("OPENED"));
            json(if leaked { 200 } else { 403 }, format!("[{}]", results.join(",")))
        }

        // Rename `from` to `to` (default: onto itself, a POSIX no-op) `n` times.
        "/rename" => {
            let mount = format!("/{}", req.q("mount").unwrap_or("tmp"));
            let from = req.q("from").unwrap_or("fill.bin").to_string();
            let to = req.q("to").unwrap_or(&from).to_string();
            let dir = util::mount(&mount).unwrap();
            for i in 0..req.q_u64("n", 1) {
                if let Err(e) = dir.rename_at(from.clone(), dir, to.clone()).await {
                    return json(507, format!("{{\"renamed\":{i},\"error\":{}}}", js(&util::fs_err(&e))));
                }
            }
            json(200, "{\"renamed\":true}")
        }

        "/cross-rename" => {
            let _ = util::write_file("/tmp", "x.txt", b"x".to_vec()).await;
            let tmp = util::mount("/tmp").unwrap();
            let data = util::mount("/data").unwrap();
            match tmp.rename_at("x.txt".to_string(), data, "x.txt".to_string()).await {
                Ok(()) => json(200, "{\"renamed\":true}"),
                Err(e) => fs_fail(0, &e),
            }
        }

        // Outbound HTTP to an arbitrary URL.
        "/fetch" => {
            let url = req.q("url").unwrap_or("http://169.254.169.254/latest/meta-data/").to_string();
            let method = req.q("method").unwrap_or("GET").to_string();
            match fetch(&url, &method).await {
                Ok((status, body)) => json(
                    200,
                    format!("{{\"status\":{status},\"bytes\":{},\"body\":{}}}", body.len(), js(&String::from_utf8_lossy(&body[..body.len().min(200)]))),
                ),
                Err(e) => json(502, format!("{{\"denied\":{}}}", js(&format!("{e:?}")))),
            }
        }

        // Hold `n` host resources forever.
        "/leak" => {
            let n = req.q_u64("n", 1_000_000);
            for _ in 0..n {
                LEAKED.with(|l| l.borrow_mut().push(Fields::new()));
            }
            json(200, format!("{{\"leaked\":{}}}", LEAKED.with(|l| l.borrow().len())))
        }

        _ => text(404, "not found"),
    }
}

fn fs_fail(written: u64, e: &util::FsErrorCode) -> Result<Response, ErrorCode> {
    json(507, format!("{{\"written\":{written},\"error\":{}}}", js(&util::fs_err(e))))
}

async fn fetch(url: &str, method: &str) -> Result<(u16, Vec<u8>), ErrorCode> {
    let (scheme, rest) = match url.split_once("://") {
        Some(("https", r)) => (Scheme::Https, r),
        Some(("http", r)) => (Scheme::Http, r),
        _ => return Err(ErrorCode::HttpRequestUriInvalid),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (trailers_tx, trailers_rx) = wit_future::new(|| Ok(None));
    let (request, _sent) = Request::new(Fields::new(), None, trailers_rx, None);
    drop(trailers_tx);
    let m = match method {
        "GET" => Method::Get,
        "POST" => Method::Post,
        "DELETE" => Method::Delete,
        "PUT" => Method::Put,
        other => Method::Other(other.to_string()),
    };
    let _ = request.set_method(&m);
    let _ = request.set_scheme(Some(&scheme));
    let _ = request.set_authority(Some(authority));
    let _ = request.set_path_with_query(Some(path));
    let response = wasip3::http::client::send(request).await?;
    let status = response.get_status_code();
    let (result_tx, result_rx) = wit_future::new(|| Ok(()));
    let (body_rx, _trailers) = Response::consume_body(response, result_rx);
    let body = body_rx.collect().await;
    drop(result_tx);
    Ok((status, body))
}
