//! Hostile guest, second edition (D-58): test-only, never part of the
//! published guest workspace (`guests/**`). Each route is an attack the host
//! must contain; `crates/nanovm/tests/hostile2.rs` drives them.
use std::hint::black_box;

use util::wasip3;
use wasip3::filesystem::types::{Descriptor, DescriptorFlags, DescriptorType, OpenFlags, PathFlags};
use wasip3::http::types::{ErrorCode, Fields, Method, Request, Response, Scheme};
use wasip3::{wit_future, wit_stream};

wasip3::http::service::export!(Hostile2);
struct Hostile2;

impl wasip3::exports::http::handler::Guest for Hostile2 {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let req = util::read_req(request).await;
        let n = |k: &str, d: u64| req.q_u64(k, d);
        let res = match req.path.as_str() {
            "/" => util::text(200, "ok"),
            "/recurse" => util::text(200, recurse(black_box(n("depth", u64::MAX))).to_string()),
            "/regex" => util::text(200, plus_plus(("a".repeat(n("n", 32) as usize) + "!").as_bytes()).to_string()),
            "/hashflood" => hashflood(n("n", 1000) as usize, req.q("collide") == Some("1"), n("reps", 20)),
            "/headers" => headers(n("n", 100) as usize),
            "/storm" => storm(req.q("url").unwrap_or_default(), n("n", 50)).await,
            "/renames" => renames(n("n", 1000), req.q("then") == Some("fail")).await,
            "/deep" => deep(n("depth", 20) as usize, n("len", 255) as usize, req.q("op").unwrap_or("create"), req.q("then") == Some("fail")).await,
            "/names" => names(req.q("then") == Some("fail")).await,
            "/ls" => util::text(200, ls().await),
            "/upload" => util::text(200, req.body.len().to_string()),
            _ => util::text(404, "not found"),
        };
        res
    }
}

fn fail(what: impl Into<String>) -> Result<Response, ErrorCode> {
    Err(ErrorCode::InternalError(Some(what.into())))
}

/// Unbounded recursion: the host must trap it, not crash.
#[inline(never)]
fn recurse(n: u64) -> u64 {
    let frame = black_box([n; 32]);
    if n == 0 { 0 } else { black_box(recurse(black_box(frame[7] - 1))) + 1 }
}

/// `(a+)+$` by backtracking, as a naive regex engine matches it: on `a…a!`
/// it tries every way to split the a's, 2^n of them.
fn plus_plus(s: &[u8]) -> bool {
    (1..=s.len()).any(|i| s[..i].iter().all(|&c| c == b'a') && (i == s.len() || plus_plus(&s[i..])))
}

const FNV_PRIME: u64 = 0x100000001b3;
const MASK: u64 = (1 << 15) - 1;

fn fnv(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h = (h ^ u64::from(b)).wrapping_mul(FNV_PRIME);
    }
    h
}

/// `n` header names that all land in the same bucket of `http::HeaderMap`'s
/// fast (FNV) hash: its low 15 bits, over the derived discriminant of a
/// custom name and the name. The last two bytes are solved for: the low 15
/// bits of each step depend only on the low 15 bits before it.
fn colliding(n: usize) -> Vec<String> {
    let chars: Vec<u8> = (b'a'..=b'z').chain(b'0'..=b'9').collect();
    // P's inverse modulo 2^64 (Newton), so a target state can be unwound.
    let mut inv = FNV_PRIME;
    for _ in 0..6 {
        inv = inv.wrapping_mul(2u64.wrapping_sub(FNV_PRIME.wrapping_mul(inv)));
    }
    let target = 0x1234 & MASK;
    let z = target.wrapping_mul(inv) & MASK;
    let mut out = Vec::with_capacity(n);
    let mut counter = 0u64;
    while out.len() < n {
        counter += 1;
        let prefix = format!("x{counter:x}");
        let s = fnv(fnv(0xcbf29ce484222325, &1isize.to_ne_bytes()), prefix.as_bytes());
        for &b1 in &chars {
            let y = (s ^ u64::from(b1)).wrapping_mul(FNV_PRIME) & MASK;
            let b2 = (y ^ z) & MASK;
            if b2 < 256 && chars.contains(&(b2 as u8)) {
                out.push(format!("{prefix}{}{}", b1 as char, b2 as u8 as char));
                break;
            }
        }
    }
    out
}

/// Builds a host header map of `n` names `reps` times and reports the
/// median nanoseconds per build: colliding names, or ordinary ones.
fn hashflood(n: usize, collide: bool, reps: u64) -> Result<Response, ErrorCode> {
    let names = if collide { colliding(n) } else { (0..n).map(|i| format!("x{i:x}zz")).collect() };
    if collide {
        let s = fnv(0xcbf29ce484222325, &1isize.to_ne_bytes());
        let first = fnv(s, names[0].as_bytes()) & MASK;
        assert!(names.iter().all(|k| fnv(s, k.as_bytes()) & MASK == first));
    }
    let list: Vec<(String, Vec<u8>)> = names.into_iter().map(|k| (k, b"v".to_vec())).collect();
    let mut times = vec![];
    for _ in 0..reps {
        let t0 = wasip3::clocks::monotonic_clock::now();
        let fields = Fields::from_list(&list).map_err(|e| ErrorCode::InternalError(Some(format!("{e:?}"))))?;
        times.push(wasip3::clocks::monotonic_clock::now() - t0);
        drop(black_box(fields));
    }
    times.sort();
    util::text(200, times[times.len() / 2].to_string())
}

/// A response with `n` headers, or as many as the host takes.
fn headers(n: usize) -> Result<Response, ErrorCode> {
    let fields = Fields::new();
    let mut set = 0;
    for i in 0..n {
        if fields.append(&format!("x-h{i}"), b"v").is_err() {
            break;
        }
        set += 1;
    }
    let (mut tx, rx) = wit_stream::new();
    let (trailers_tx, trailers_rx) = wit_future::new(|| Ok(None));
    drop(trailers_tx);
    let (response, _) = Response::new(fields, Some(rx), trailers_rx);
    wasip3::spawn_local(async move {
        let _ = tx.write_all(set.to_string().into_bytes()).await;
    });
    Ok(response)
}

/// `n` outbound requests to `url` at once; how many were answered, and how
/// many refused.
async fn storm(url: &str, n: u64) -> Result<Response, ErrorCode> {
    let authority = url.strip_prefix("http://").unwrap_or(url).trim_end_matches('/').to_string();
    let sends = (0..n).map(|_| {
        let authority = authority.clone();
        async move {
            let (trailers_tx, trailers_rx) = wit_future::new(|| Ok(None));
            drop(trailers_tx);
            let (request, _) = Request::new(Fields::new(), None, trailers_rx, None);
            let _ = request.set_method(&Method::Get);
            let _ = request.set_scheme(Some(&Scheme::Http));
            let _ = request.set_authority(Some(&authority));
            let _ = request.set_path_with_query(Some("/"));
            match wasip3::http::client::send(request).await {
                Ok(r) => Ok(r.get_status_code()),
                Err(e) => Err(format!("{e:?}")),
            }
        }
    });
    let results = futures::future::join_all(sends).await;
    let ok = results.iter().filter(|r| r.is_ok()).count();
    let refused = results.iter().filter(|r| matches!(r, Err(e) if e.contains("ConnectionLimitReached"))).count();
    util::text(200, format!("{ok} {refused} {}", results.len() - ok - refused))
}

/// Renames `f` to `g` and back `n` times in one request (each is journaled),
/// then answers or fails.
async fn renames(n: u64, then_fail: bool) -> Result<Response, ErrorCode> {
    let dir = util::mount("/data").expect("/data");
    let _ = util::write_file("/data", "f", b"renamed".to_vec()).await;
    let mut done = 0;
    let mut error = String::new();
    for i in 0..n {
        let (a, b) = if i % 2 == 0 { ("f", "g") } else { ("g", "f") };
        match dir.rename_at(a.into(), dir, b.into()).await {
            Ok(()) => done += 1,
            Err(e) => {
                error = util::fs_err(&e);
                break;
            }
        }
    }
    let report = format!("{done} {error}");
    if then_fail { fail(report) } else { util::text(200, report) }
}

/// Opens (creating, with `create`) a chain of `depth` directories named
/// with `len` repetitions of a letter, each from its parent's descriptor, so
/// no path the guest passes is long but the volume's path to the bottom is.
/// Then `op` on the file `x` at the bottom: `create` writes it, `overwrite`
/// rewrites it, `unlink` removes it. Answers or fails afterwards.
async fn deep(depth: usize, len: usize, op: &str, then_fail: bool) -> Result<Response, ErrorCode> {
    let flags = DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY;
    let mut at: Descriptor = util::open("/data", ".", OpenFlags::DIRECTORY, flags).await.map_err(|e| fs(&e))?;
    for i in 0..depth {
        let name = ((b'a' + (i % 26) as u8) as char).to_string().repeat(len);
        if op == "create" {
            match at.create_directory_at(name.clone()).await {
                Ok(()) | Err(wasip3::filesystem::types::ErrorCode::Exist) => {}
                Err(e) => return fail(format!("mkdir at depth {i}: {}", util::fs_err(&e))),
            }
        }
        at = match at.open_at(PathFlags::empty(), name, OpenFlags::DIRECTORY, flags).await {
            Ok(d) => d,
            Err(e) => return fail(format!("open at depth {i}: {}", util::fs_err(&e))),
        };
    }
    let result = match op {
        "create" | "overwrite" => {
            let content = if op == "create" { b"committed".to_vec() } else { b"ROLLED-BACK?".to_vec() };
            let wf = DescriptorFlags::READ | DescriptorFlags::WRITE;
            match at.open_at(PathFlags::empty(), "x".into(), OpenFlags::CREATE | OpenFlags::TRUNCATE, wf).await {
                Ok(f) => util::write_all(&f, content, 0).await,
                Err(e) => Err(e),
            }
        }
        "unlink" => at.unlink_file_at("x".into()).await,
        "read" => {
            let f = at.open_at(PathFlags::empty(), "x".into(), OpenFlags::empty(), DescriptorFlags::READ).await;
            return match f {
                Ok(f) => {
                    let (rx, done) = f.read_via_stream(0);
                    let data = rx.collect().await;
                    let _ = done.await;
                    util::text(200, String::from_utf8_lossy(&data).into_owned())
                }
                Err(e) => util::text(404, util::fs_err(&e)),
            };
        }
        other => return fail(format!("unknown op {other}")),
    };
    let report = match result {
        Ok(()) => "ok".to_string(),
        Err(e) => util::fs_err(&e),
    };
    if then_fail { fail(report) } else { util::text(200, report) }
}

fn fs(e: &wasip3::filesystem::types::ErrorCode) -> ErrorCode {
    ErrorCode::InternalError(Some(util::fs_err(e)))
}

/// Unusual names: the longest a name may be, one byte longer, combining
/// marks in both normal forms, a right-to-left override, zero-width and
/// emoji characters, a newline, and names that look like `.` and `..` but
/// aren't. Each is written with its own name as content.
pub const NAMES: &[&str] = &[
    "\u{e9}", "e\u{301}", "\u{202e}gnp.exe", "a\u{200b}b", "\u{1f600}\u{1f3fb}", "line\nbreak", ".\u{200b}", "..\u{301}",
    " leading space", "trailing.", "\\back\\slash", "\u{ff0e}\u{ff0e}",
];

async fn names(then_fail: bool) -> Result<Response, ErrorCode> {
    let mut report = vec![];
    let long = "n".repeat(255);
    let too_long = "n".repeat(256);
    let long4 = "\u{1f600}".repeat(63); // 252 bytes in 63 characters
    let all: Vec<String> = NAMES.iter().map(|s| s.to_string()).chain([long, too_long, long4]).collect();
    for name in &all {
        let r = util::write_file("/data", name, name.as_bytes().to_vec()).await;
        report.push(match r {
            Ok(()) => "ok".to_string(),
            Err(e) => util::fs_err(&e),
        });
    }
    let report = report.join(",");
    if then_fail { fail(report) } else { util::text(200, report) }
}

/// Every entry under `/data` as `path<TAB>size`, separated by NULs (names
/// may hold newlines; directories have size `-`), sorted.
async fn ls() -> String {
    let mut out = vec![];
    let mut stack = vec![String::new()];
    while let Some(dir) = stack.pop() {
        let path = if dir.is_empty() { ".".to_string() } else { dir.clone() };
        let Ok(fd) = util::open("/data", &path, OpenFlags::DIRECTORY, DescriptorFlags::READ).await else { continue };
        let (rx, done) = fd.read_directory();
        let entries = rx.collect().await;
        let _ = done.await;
        for e in entries {
            let p = if dir.is_empty() { e.name.clone() } else { format!("{dir}/{}", e.name) };
            if matches!(e.type_, DescriptorType::Directory) {
                out.push(format!("{p}\t-"));
                stack.push(p);
            } else {
                let size = match fd.stat_at(PathFlags::empty(), e.name.clone()).await {
                    Ok(st) => st.size.to_string(),
                    Err(e) => util::fs_err(&e),
                };
                out.push(format!("{p}\t{size}"));
            }
        }
    }
    out.sort();
    out.join("\0")
}
