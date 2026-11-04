//! Test-only component; never included in the published guest workspace.
use std::cell::RefCell;

use nanovm_sdk::wasip3::{self, http::types::{ErrorCode, Fields, Request, Response}};
use nanovm_sdk::{data, fetch, object};

wasip3::http::service::export!(Probe);
struct Probe;

thread_local! {
    static HELD: RefCell<Vec<Fields>> = RefCell::default();
}

impl wasip3::exports::http::handler::Guest for Probe {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        if request.get_path_with_query().is_some_and(|p| p.starts_with("/headers")) {
            return headers(request);
        }
        let req = nanovm_sdk::Request::read(request).await;
        if req.path() == "/callee" {
            return nanovm_sdk::Response::text("callee").into_wit();
        }
        let gate = req.query("gate").unwrap().to_owned();
        let (mut tx, rx) = wasip3::wit_stream::new();
        let (trailers, trailers_rx) = wasip3::wit_future::new(|| Ok(None));
        drop(trailers);
        let (response, _) = Response::new(Fields::new(), Some(rx), trailers_rx);
        response.set_status_code(200).unwrap();
        let post = move |path: &str| fetch(format!("http://{gate}{path}")).method("POST").body(vec![]).send();
        wasip3::spawn_local(async move {
            // The test does not release this gate until it has the headers.
            assert_eq!(post("/begin").await.unwrap().status, 200);
            eprintln!("post-return diagnostic");
            assert_eq!(object("child").post("/callee", vec![]).send().await.unwrap().status, 200);
            data().write("after.txt", "after").await.unwrap();
            assert_eq!(post("/finish").await.unwrap().status, 200);
            assert!(tx.write_all(b"finished".to_vec()).await.is_empty());
        });
        Ok(response)
    }
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
    nanovm_sdk::Response::text(HELD.with(|h| h.borrow().len()).to_string()).into_wit()
}
