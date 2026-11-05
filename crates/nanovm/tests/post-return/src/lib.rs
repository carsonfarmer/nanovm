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
        if req.path == "/retained" {
            return retained(req.q("gate").unwrap().to_owned()).await;
        }
        if req.path == "/retained-reject" {
            util::write_file("/data", "read", b"rejected".to_vec()).await.unwrap();
            util::mount("/data").unwrap().unlink_file_at("extra".into()).await.unwrap();
            return Err(ErrorCode::InternalError(Some("reject candidate".into())));
        }
        if req.path == "/retained-state" {
            let read = util::read_file("/data", "read").await.unwrap();
            let extra = util::read_file("/data", "extra").await.unwrap();
            return util::text(200, format!("{}/{}", String::from_utf8(read).unwrap(), String::from_utf8(extra).unwrap()));
        }
        if req.path == "/state" {
            let mut state = Vec::new();
            for file in ["before.txt", "after.txt", "other.txt"] {
                state.push(String::from_utf8(util::read_file("/data", file).await.unwrap()).unwrap());
            }
            return util::text(200, state.join("/"));
        }
        for file in ["before.txt", "after.txt", "other.txt"] {
            util::write_file("/data", file, b"old".to_vec()).await.unwrap();
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
            util::write_file("/data", "other.txt", b"other".to_vec()).await.unwrap();
            assert_eq!(util::post(&gate, "/finish", vec![]).await.unwrap(), 200);
            assert!(tx.write_all(b"finished".to_vec()).await.is_empty());
        });
        Ok(response)
    }
}

async fn retained(gate: String) -> Result<Response, ErrorCode> {
    let root = util::mount("/data").unwrap();
    util::write_file("/data", "read", b"old".to_vec()).await.unwrap();
    let fd = util::open("/data", "read", util::OFlags::empty(), util::FdFlags::READ).await.unwrap();
    let (read, read_done) = fd.read_via_stream(0);
    drop(fd);
    root.unlink_file_at("read".into()).await.unwrap();

    let fd = util::open("/data", "write", util::OFlags::CREATE, util::FdFlags::READ | util::FdFlags::WRITE).await.unwrap();
    let (mut input, output) = wasip3::wit_stream::new();
    let write_done = fd.write_via_stream(output, 0);
    let (written, written_done) = fd.read_via_stream(0);
    drop(fd);
    root.unlink_file_at("write".into()).await.unwrap();

    root.create_directory_at("directory".into()).await.unwrap();
    let fd = util::open("/data", "directory", util::OFlags::DIRECTORY, util::FdFlags::READ).await.unwrap();
    let (directory, directory_done) = fd.read_directory();
    drop(fd);
    root.remove_directory_at("directory".into()).await.unwrap();

    util::write_file("/data", "read", b"new".to_vec()).await.unwrap();
    // Four entries: two held files, a held directory, and the replacement.
    assert!(matches!(util::write_file("/data", "extra", b"x".to_vec()).await, Err(util::FsErrorCode::Quota)));
    let (mut body, rx) = wasip3::wit_stream::new();
    let (trailers, trailers_rx) = wasip3::wit_future::new(|| Ok(None));
    drop(trailers);
    let (response, _) = Response::new(Fields::new(), Some(rx), trailers_rx);
    response.set_status_code(200).unwrap();
    wasip3::spawn_local(async move {
        // The host releases this only after accepting the response headers.
        assert_eq!(util::post(&gate, "/begin", vec![]).await.unwrap(), 200);
        assert!(input.write_all(b"written".to_vec()).await.is_empty());
        drop(input);
        write_done.await.unwrap();
        assert_eq!(written.collect().await, b"written");
        written_done.await.unwrap();
        assert_eq!(read.collect().await, b"old");
        read_done.await.unwrap();
        assert!(directory.collect().await.is_empty());
        directory_done.await.unwrap();
        // Only final stream destruction can reclaim the orphan quota.
        for name in ["extra", "extra2", "extra3"] {
            util::write_file("/data", name, b"x".to_vec()).await.unwrap();
        }
        assert!(matches!(util::write_file("/data", "overflow", b"x".to_vec()).await, Err(util::FsErrorCode::Quota)));
        assert!(body.write_all(b"retained".to_vec()).await.is_empty());
    });
    Ok(response)
}
