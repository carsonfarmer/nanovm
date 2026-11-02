//! Test-only component; never included in the published guest workspace.
use util::wasip3;
use wasip3::http::types::{ErrorCode, Fields, Request, Response};

wasip3::http::service::export!(Probe);
struct Probe;

impl wasip3::exports::http::handler::Guest for Probe {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let req = util::read_req(request).await;
        if req.path == "/callee" {
            return util::text(200, "callee");
        }
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
