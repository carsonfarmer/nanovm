//! Test-only component; never included in the published guest workspace.
use std::cell::RefCell;

use util::wasip3;
use wasip3::http::types::{ErrorCode, Fields, Method, Request, Response};

wasip3::http::service::export!(Probe);
struct Probe;

thread_local! {
    static HELD: RefCell<Vec<Fields>> = RefCell::default();
    static REQUESTS: RefCell<Vec<(Request, usize)>> = RefCell::default();
}

impl wasip3::exports::http::handler::Guest for Probe {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        if request.get_path_with_query().is_some_and(|p| p.starts_with("/headers")) {
            return headers(request);
        }
        // The callers the node says wait on this request (W-602).
        if request.get_path_with_query().as_deref() == Some("/callers") {
            let seen: Vec<_> = request.get_headers().get("nanovm-callers").iter().map(|v| String::from_utf8_lossy(v).into_owned()).collect();
            return util::text(200, seen.join(";"));
        }
        // Calls `/callers` on another object, claiming callers of its own.
        if let Some(target) = request.get_path_with_query().as_deref().and_then(|p| p.strip_prefix("/call/")) {
            return call(target).await;
        }
        if request.get_path_with_query().is_some_and(|p| p.starts_with("/strings")) {
            return strings(request);
        }
        let req = util::read_req(request).await;
        if req.path == "/callee" {
            return util::text(200, "callee");
        }
        // Answers as a node refusing the object would, naming another.
        if let Some(owner) = req.path.strip_prefix("/misdirect/") {
            return util::respond_with(421, &[("nanovm-owner", owner)], b"forged".to_vec());
        }
        if req.path == "/selfwake" {
            return selfwake().await;
        }
        util::write_file("/data", "before.txt", b"before".to_vec()).await.unwrap();
        let gate = req.q("gate").unwrap().to_owned();
        let (mut tx, rx) = wasip3::wit_stream::new();
        let (trailers, trailers_rx) = wasip3::wit_future::new(|| Ok(None));
        drop(trailers);
        let (response, _) = Response::new(Fields::new(), Some(rx), trailers_rx);
        response.set_status_code(200).unwrap();
        wasip3::spawn_local(async move {
            // The test does not release this gate until it has the headers.
            assert_eq!(util::post(&gate, "/begin", vec![]).await.unwrap(), 200);
            eprintln!("post-return diagnostic");
            assert_eq!(util::post("nanovm", "/objects/child/callee", vec![]).await.unwrap(), 200);
            util::write_file("/data", "after.txt", b"after".to_vec()).await.unwrap();
            assert_eq!(util::post(&gate, "/finish", vec![]).await.unwrap(), 200);
            assert!(tx.write_all(b"finished".to_vec()).await.is_empty());
        });
        Ok(response)
    }
}

async fn call(target: &str) -> Result<Response, ErrorCode> {
    use wasip3::{wit_future, http::types::Scheme};
    let forged = Fields::from_list(&[("nanovm-callers".into(), b"forged".to_vec())]).map_err(|e| ErrorCode::InternalError(Some(format!("{e:?}"))))?;
    let (trailers_tx, trailers_rx) = wit_future::new(|| Ok(None));
    let (request, _sent) = Request::new(forged, None, trailers_rx, None);
    drop(trailers_tx);
    let _ = request.set_scheme(Some(&Scheme::Http));
    let _ = request.set_authority(Some("nanovm"));
    let _ = request.set_path_with_query(Some(&format!("/objects/{target}/callers")));
    let response = wasip3::http::client::send(request).await?;
    let (result_tx, result_rx) = wit_future::new(|| Ok(()));
    let (body, _trailers) = Response::consume_body(response, result_rx);
    let body = body.collect().await;
    drop(result_tx);
    util::text(200, String::from_utf8_lossy(&body).into_owned())
}

/// `/selfwake`: three tasks that wake themselves in a tight loop, forever.
/// Each wake hands control back to the host (`CALLBACK_CODE_YIELD`), which
/// runs it again at once.
async fn selfwake() -> Result<Response, ErrorCode> {
    let spin = || async {
        loop {
            wasip3::wit_bindgen::yield_async().await;
        }
    };
    wasip3::spawn_local(spin());
    wasip3::spawn_local(spin());
    spin().await;
    unreachable!()
}

/// `/headers?n=…&kb=…`: holds `n` more header maps, for good: new ones with
/// a `kb` KiB value, or (without `kb`) copies of the request's own. Answers
/// how many it holds.
fn headers(request: Request) -> Result<Response, ErrorCode> {
    let q = request.get_path_with_query().unwrap_or_default();
    let arg = |k: &str| q.split(['?', '&']).find_map(|kv| kv.strip_prefix(k)?.strip_prefix('=')?.parse().ok());
    let (n, kb): (usize, Option<usize>) = (arg("n").unwrap_or(0), arg("kb"));
    let fail = |e| ErrorCode::InternalError(Some(format!("{e:?}")));
    let (theirs, value) = (request.get_headers(), vec![b'x'; kb.unwrap_or(0) << 10]);
    for _ in 0..n {
        let held = if kb.is_some() { Fields::new() } else { theirs.clone() };
        // Changing a copy makes it one of its own.
        held.append("x-held", &value).map_err(fail)?;
        HELD.with(|h| h.borrow_mut().push(held));
    }
    util::text(200, HELD.with(|h| h.borrow().len()).to_string())
}

/// `/strings?part=…&len=…&n=…&hlen=…`: makes `n` requests, each with an
/// `hlen`-byte header and a `len`-byte `method`, `path` or `authority`
/// (`part`), and holds those that took it, for good. Answers the bytes they
/// hold in those parts and headers, and how many refused it.
fn strings(request: Request) -> Result<Response, ErrorCode> {
    let q = request.get_path_with_query().unwrap_or_default();
    let arg = |k: &str| q.split(['?', '&']).find_map(|kv| kv.strip_prefix(k)?.strip_prefix('='));
    let num = |k: &str| arg(k).and_then(|v| v.parse().ok()).unwrap_or(0usize);
    let (part, len, n, hlen) = (arg("part").unwrap_or_default(), num("len"), num("n"), num("hlen"));
    let fail = |e| ErrorCode::InternalError(Some(format!("{e:?}")));
    let mut refused = 0;
    for _ in 0..n {
        let headers = Fields::new();
        if hlen > 0 {
            headers.append("x-held", &vec![b'x'; hlen]).map_err(fail)?;
        }
        let (trailers_tx, trailers_rx) = wasip3::wit_future::new(|| Ok(None));
        drop(trailers_tx);
        let (req, _) = Request::new(headers, None, trailers_rx, None);
        let took = match part {
            "method" => req.set_method(&Method::Other("X".repeat(len))),
            "path" => req.set_path_with_query(Some(&format!("/{}", "p".repeat(len.max(1) - 1)))),
            "authority" => req.set_authority(Some(&"a".repeat(len))),
            _ => return util::text(400, "part"),
        };
        match took {
            Ok(()) => REQUESTS.with(|r| r.borrow_mut().push((req, len + hlen))),
            Err(()) => refused += 1,
        }
    }
    let held: usize = REQUESTS.with(|r| r.borrow().iter().map(|(_, bytes)| bytes).sum());
    util::text(200, format!("{held} {refused}"))
}
