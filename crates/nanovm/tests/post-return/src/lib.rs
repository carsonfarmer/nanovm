//! Test-only component; never included in the published guest workspace.
use nanovm_sdk::wasip3::{self, http::types::{ErrorCode, Fields, Request, Response}};
use nanovm_sdk::{data, fetch, object};

wasip3::http::service::export!(Probe);
struct Probe;

impl wasip3::exports::http::handler::Guest for Probe {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
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
