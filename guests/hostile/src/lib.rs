//! A deliberately hostile component. Every endpoint tries to break out of, or
//! exhaust, one of the host's limits. The host is expected to contain each one.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nanovm_sdk::fs::{self, DescriptorFlags as FdFlags, Dir, ErrorCode as FsError, OpenFlags, PathFlags};
use nanovm_sdk::wasip3::http::types::Fields;
use nanovm_sdk::{Error, Request, Response, Result, data, kv, quote, sleep, tmp};

nanovm_sdk::serve!(handle);

static HITS: AtomicU64 = AtomicU64::new(0);
static ACTIVE: AtomicU64 = AtomicU64::new(0);
static MAX_ACTIVE: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static HOARD: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
    static LEAKED: RefCell<Vec<Fields>> = const { RefCell::new(Vec::new()) };
}

async fn handle(req: Request) -> Result {
    HITS.fetch_add(1, Ordering::Relaxed);
    let now = ACTIVE.fetch_add(1, Ordering::SeqCst) + 1;
    MAX_ACTIVE.fetch_max(now, Ordering::SeqCst);
    let res = route(req).await;
    ACTIVE.fetch_sub(1, Ordering::SeqCst);
    res
}

async fn route(req: Request) -> Result {
    Ok(match req.path() {
        "/" => Response::json(format!(r#"{{"object":{},"hits":{}}}"#, quote(&nanovm_sdk::id()), HITS.load(Ordering::Relaxed))),
        "/echo" => Response::bytes("application/octet-stream", req.into_body()),
        // Writes `v` to `/data/c.txt` (in place) and to the key `c`, then
        // ends as `then` says: ok, fail (507), error, panic, spin or sleep.
        "/commit" => {
            let v = req.query("v").unwrap_or("");
            if !v.is_empty() {
                let _ = data().write("c.txt", v).await;
                let _ = kv::set("c", v);
            }
            match req.query("then").unwrap_or("ok") {
                "fail" => Response::json("{}").status(507),
                "panic" => panic!("after writing"),
                "error" => return Err(Error::new("after writing")),
                "spin" => loop {
                    std::hint::black_box(0);
                },
                "sleep" => {
                    sleep(Duration::from_secs(60)).await;
                    Response::json("{}")
                }
                _ => {
                    let file = data().read_text("c.txt").await.ok().flatten().unwrap_or_default();
                    let key = kv::get("c").ok().flatten().unwrap_or_default();
                    Response::json(format!(r#"{{"file":{},"key":{}}}"#, quote(&file), quote(&String::from_utf8_lossy(&key))))
                }
            }
        }
        // A body big enough that a client can stop reading it part way.
        "/big" => Response::bytes("application/octet-stream", vec![b'x'; req.query_or::<usize>("mb", 4) << 20]),

        // Grow linear memory by `mb` MiB, touching every page, and keep it.
        "/alloc" => {
            for _ in 0..req.query_or::<u64>("mb", 1024) {
                let mut v = vec![0u8; 1 << 20];
                for i in (0..v.len()).step_by(4096) {
                    v[i] = 1;
                }
                HOARD.with(|h| h.borrow_mut().push(v));
            }
            Response::json(format!(r#"{{"held_mb":{}}}"#, HOARD.with(|h| h.borrow().len())))
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
            let n = req.query_or("ms", 60_000);
            sleep(Duration::from_millis(n)).await;
            Response::json(format!(r#"{{"slept_ms":{n}}}"#))
        }

        "/panic" => panic!("hostile guest panicked on purpose"),

        // Concurrency probe: hold the request for `ms`, report max overlap seen.
        "/concurrency" => {
            sleep(Duration::from_millis(req.query_or("ms", 20))).await;
            Response::json(format!(r#"{{"max_active":{}}}"#, MAX_ACTIVE.load(Ordering::SeqCst)))
        }

        // Write 1 MiB chunks to `/{mount}/fill.bin` until `mb` or an error.
        "/fill" => {
            let name = req.query("name").unwrap_or("fill.bin");
            let fd = match async { mount(&req)?.open(name, OpenFlags::CREATE, FdFlags::READ | FdFlags::WRITE).await }.await {
                Ok(fd) => fd,
                Err(e) => return Ok(fs_fail(0, &e)),
            };
            let mut written = 0u64;
            for i in 0..req.query_or("mb", 1 << 20) {
                if let Err(e) = fs::write_at(&fd, vec![0xAB; 1 << 20], i << 20).await {
                    return Ok(fs_fail(fd.stat().await.map(|s| s.size).unwrap_or(written), &e));
                }
                written += 1 << 20;
            }
            Response::json(format!(r#"{{"written":{written}}}"#))
        }

        // One file of `mb` MiB in a single write.
        "/bigfile" => {
            let mb = req.query_or::<u64>("mb", 64);
            match async { mount(&req)?.write("big.bin", vec![7u8; (mb as usize) << 20]).await }.await {
                Ok(()) => Response::json(format!(r#"{{"written":{}}}"#, mb << 20)),
                Err(e) => fs_fail(0, &e),
            }
        }

        // Create `n` small files until the file-count limit.
        "/files" => {
            let n = req.query_or::<u64>("n", 1_000_000);
            for i in 0..n {
                if let Err(e) = async { mount(&req)?.write(&format!("f{i}"), "x").await }.await {
                    return Ok(Response::json(format!(r#"{{"created":{i},"error":{}}}"#, quote(&fs::code(&e)))).status(507));
                }
            }
            Response::json(format!(r#"{{"created":{n}}}"#))
        }

        // Delete files created by /fill, /bigfile and /files.
        "/clean" => {
            let dir = mount(&req).unwrap();
            let mut removed = 0;
            let names = ["fill.bin".to_string(), "big.bin".to_string()];
            for name in names.into_iter().chain((0..req.query_or::<u64>("n", 0)).map(|i| format!("f{i}"))) {
                if dir.remove(&name).await.is_ok() {
                    removed += 1;
                }
            }
            Response::json(format!(r#"{{"removed":{removed}}}"#))
        }

        // Try to read outside the sandbox in every way we can think of.
        "/escape" => {
            let mut results = Vec::new();
            for (mount, path) in [
                ("/data", "../../../../../../etc/passwd"),
                ("/data", "/etc/passwd"),
                ("/tmp", "../"),
                ("/tmp", "../../data"),
            ] {
                let result = match Dir::at(mount).unwrap().read(path).await {
                    Ok(Some(b)) => format!("LEAKED {} bytes", b.len()),
                    Ok(None) => fs::code(&FsError::NoEntry),
                    Err(e) => fs::code(&e),
                };
                results.push(format!(r#"{{"mount":{},"path":{},"result":{}}}"#, quote(mount), quote(path), quote(&result)));
            }
            // Symlink pointing outside, then follow it.
            let data = data().descriptor();
            let link = data.symlink_at("/etc/passwd".into(), "evil-link".into()).await;
            let follow = data.open_at(PathFlags::SYMLINK_FOLLOW, "evil-link".into(), OpenFlags::empty(), FdFlags::READ).await;
            let _ = data.unlink_file_at("evil-link".into()).await;
            let link = link.map_or_else(|e| fs::code(&e), |()| "created".into());
            let follow = follow.map_or_else(|e| fs::code(&e), |_| "OPENED".into());
            results.push(format!(r#"{{"symlink":{},"follow":{}}}"#, quote(&link), quote(&follow)));
            let leaked = results.iter().any(|r| r.contains("LEAKED") || r.contains("OPENED"));
            Response::json(format!("[{}]", results.join(","))).status(if leaked { 200 } else { 403 })
        }

        // Rename `from` to `to` (default: onto itself, a POSIX no-op) `n` times.
        "/rename" => {
            let from = req.query("from").unwrap_or("fill.bin");
            let to = req.query("to").unwrap_or(from);
            let dir = mount(&req).unwrap().descriptor();
            for i in 0..req.query_or::<u64>("n", 1) {
                if let Err(e) = dir.rename_at(from.into(), dir, to.into()).await {
                    return Ok(Response::json(format!(r#"{{"renamed":{i},"error":{}}}"#, quote(&fs::code(&e)))).status(507));
                }
            }
            Response::json("{\"renamed\":true}")
        }

        "/cross-rename" => {
            let _ = tmp().write("x.txt", "x").await;
            match tmp().descriptor().rename_at("x.txt".into(), data().descriptor(), "x.txt".into()).await {
                Ok(()) => Response::json("{\"renamed\":true}"),
                Err(e) => fs_fail(0, &e),
            }
        }

        // Outbound HTTP to an arbitrary URL.
        "/fetch" => {
            let url = req.query("url").unwrap_or("http://169.254.169.254/latest/meta-data/");
            match nanovm_sdk::fetch(url).method(req.query("method").unwrap_or("GET")).send().await {
                Ok(r) => Response::json(format!(
                    r#"{{"status":{},"bytes":{},"body":{}}}"#,
                    r.status,
                    r.body.len(),
                    quote(&String::from_utf8_lossy(&r.body[..r.body.len().min(200)]))
                )),
                Err(e) => Response::json(format!(r#"{{"denied":{}}}"#, quote(&format!("{e:?}")))).status(502),
            }
        }

        // Send `n` requests one after another, keeping every response body
        // open and unread: each one holds an outbound connection.
        "/hold" => {
            let url = req.query("url").unwrap_or("http://example.com/");
            let mut held = Vec::new();
            for _ in 0..req.query_or::<u64>("n", 3) {
                match nanovm_sdk::fetch(url).open().await {
                    Ok(r) => held.push(r),
                    Err(e) => {
                        return Ok(Response::json(format!(r#"{{"held":{},"denied":{}}}"#, held.len(), quote(&format!("{e:?}")))));
                    }
                }
            }
            Response::json(format!(r#"{{"held":{}}}"#, held.len()))
        }

        // wasi:keyvalue: `op` is fill (n keys of `size` bytes until refused),
        // key (one key `len` bytes long), incr (`delta` on `key`), list (a
        // page from `cursor`), plant, many, or open (bucket `name`).
        "/kv" => {
            let err = |e: kv::Error| quote(&format!("{e:?}"));
            let bucket = match kv::store::open(req.query("name").unwrap_or("")) {
                Ok(b) => b,
                Err(e) => return Ok(Response::json(format!(r#"{{"opened":false,"error":{}}}"#, err(e)))),
            };
            Response::json(match req.query("op").unwrap_or("") {
                "fill" => {
                    let value = vec![7u8; req.query_or("size", 1)];
                    let (mut n, mut error) = (0u64, "null".to_string());
                    while n < req.query_or("n", 1_000_000) {
                        if let Err(e) = bucket.set(&format!("k{n}"), &value) {
                            error = err(e);
                            break;
                        }
                        n += 1;
                    }
                    format!(r#"{{"set":{n},"error":{error}}}"#)
                }
                "key" => match bucket.set(&"k".repeat(req.query_or("len", 1)), b"v") {
                    Ok(()) => r#"{"set":true}"#.into(),
                    Err(e) => format!(r#"{{"error":{}}}"#, err(e)),
                },
                "incr" => match kv::atomics::increment(&bucket, req.query("key").unwrap_or(""), req.query_or("delta", 1)) {
                    Ok(n) => format!(r#"{{"value":{n}}}"#),
                    Err(e) => format!(r#"{{"error":{}}}"#, err(e)),
                },
                "list" => match bucket.list_keys(req.query("cursor").and_then(|c| c.parse().ok())) {
                    Ok(r) => format!(r#"{{"keys":{},"cursor":{}}}"#, r.keys.len(), r.cursor.map_or("null".into(), |c| c.to_string())),
                    Err(e) => format!(r#"{{"error":{}}}"#, err(e)),
                },
                // Take key `k0`'s file name (base64url "azA") with a
                // directory through the filesystem, then set the key.
                "plant" => {
                    for d in [".kv", ".kv/default", ".kv/default/azA", ".kv/default/azA/x"] {
                        let _ = data().descriptor().create_directory_at(d.into()).await;
                    }
                    match bucket.set("k0", &vec![1u8; 1 << 20]) {
                        Ok(()) => r#"{"set":true}"#.into(),
                        Err(e) => format!(r#"{{"error":{}}}"#, err(e)),
                    }
                }
                // `get-many` of key `k0`, `n` times over.
                "many" => match kv::batch::get_many(&bucket, &vec!["k0".to_string(); req.query_or("n", 1)]) {
                    Ok(v) => format!(r#"{{"got":{}}}"#, v.iter().flatten().count()),
                    Err(e) => format!(r#"{{"error":{}}}"#, err(e)),
                },
                _ => r#"{"opened":true}"#.into(),
            })
        }

        // Hold `n` host resources forever.
        "/leak" => {
            for _ in 0..req.query_or::<u64>("n", 1_000_000) {
                LEAKED.with(|l| l.borrow_mut().push(Fields::new()));
            }
            Response::json(format!(r#"{{"leaked":{}}}"#, LEAKED.with(|l| l.borrow().len())))
        }

        _ => Response::not_found(),
    })
}

/// The mount `?mount=` names (`tmp` by default).
fn mount(req: &Request) -> core::result::Result<Dir, FsError> {
    Dir::at(&format!("/{}", req.query("mount").unwrap_or("tmp"))).ok_or(FsError::NoEntry)
}

fn fs_fail(written: u64, e: &FsError) -> Response {
    Response::json(format!(r#"{{"written":{written},"error":{}}}"#, quote(&fs::code(e)))).status(507)
}
