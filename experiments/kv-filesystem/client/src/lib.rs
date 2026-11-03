//! Same async HTTP caller tests direct P3 I/O and composed synchronous KV I/O.
use util::wasip3::http::types::{ErrorCode, Request, Response};
util::wasip3::http::service::export!(Client);
struct Client;

impl util::wasip3::exports::http::handler::Guest for Client {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let request = util::read_req(request).await;
        match request.path.as_str() {
            "/control" => {
                let error = util::kv::store::open("probe").expect_err("composed provider rejects probe");
                util::text(200, format!("{error:?}"))
            }
            "/direct" => {
                let dir = util::mount("/data").expect("data preopen");
                let _ = dir.create_directory_at(".kv".into()).await;
                let _ = dir.create_directory_at(".kv/default".into()).await;
                util::write_file("/data", ".kv/default/aw", b"filesystem-control".to_vec())
                    .await
                    .expect("direct write");
                let value = util::read_file("/data", ".kv/default/aw").await.expect("direct read");
                util::text(200, String::from_utf8(value).expect("control UTF-8"))
            }
            "/read" => {
                let bucket = util::kv::store::open("").expect("open composed bucket");
                let value = bucket.get("k").expect("composed read").expect("existing key");
                util::text(200, String::from_utf8(value).expect("control UTF-8"))
            }
            "/write" => {
                let bucket = util::kv::store::open("").expect("open composed bucket");
                bucket.set("k", b"composed-write").expect("composed write");
                util::text(200, "written")
            }
            _ => util::text(404, "unknown probe"),
        }
    }
}
