//! Test-only component for the model-based quota test
//! (`crates/nanovm/tests/quota_model.rs`); never part of the published guest
//! workspace. It runs filesystem operations on `/data` and checks each
//! result against the one the test's model expects.
//!
//! - `POST /ops?commit=1|0`: one operation per line, `<op> <args…> => <expected>`
//!   (`write <path> <offset> <len> <byte>`, `truncate <path> <size>`,
//!   `rename <from> <to>`, `unlink <path>`, `mkdir <path>`, `rmdir <path>`;
//!   expected `ok`, `insufficient-space`, `quota` or `error`). Stops at the
//!   first result that differs. Answers 200 `matched` if all matched and
//!   `commit=1` (the changes commit); otherwise fails the request (rolling it
//!   back) with `matched` or `mismatch <line> <actual>` in the error.
//! - `POST /probe?bytes=<b>&files=<f>`: checks that exactly `b` more bytes
//!   and `f` more entries fit, then fails the request so nothing stays.
//! - `GET /`: nothing (starts an instance, which recovers a rolled-back request).
use util::wasip3;
use wasip3::filesystem::types::{DescriptorFlags, ErrorCode as FsError, OpenFlags};
use wasip3::http::types::{ErrorCode, Request, Response};

wasip3::http::service::export!(Ops);
struct Ops;

impl wasip3::exports::http::handler::Guest for Ops {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let req = util::read_req(request).await;
        let report = match req.path.as_str() {
            "/ops" => {
                let report = ops(&String::from_utf8_lossy(&req.body)).await;
                if report == "matched" && req.q("commit") == Some("1") {
                    return util::text(200, report);
                }
                report
            }
            "/probe" => probe(req.q_u64("bytes", 0), req.q_u64("files", 0)).await,
            _ => return util::text(200, "idle"),
        };
        Err(ErrorCode::InternalError(Some(report)))
    }
}

fn kind(r: Result<(), FsError>) -> &'static str {
    match r {
        Ok(()) => "ok",
        Err(FsError::InsufficientSpace) => "insufficient-space",
        Err(FsError::Quota) => "quota",
        Err(_) => "error",
    }
}

fn dir() -> &'static wasip3::filesystem::types::Descriptor {
    util::mount("/data").expect("/data is mounted")
}

async fn write(path: &str, offset: u64, len: usize, byte: u8) -> Result<(), FsError> {
    let fd = util::open("/data", path, OpenFlags::CREATE, DescriptorFlags::READ | DescriptorFlags::WRITE).await?;
    if len == 0 {
        return Ok(());
    }
    util::write_all(&fd, vec![byte; len], offset).await
}

async fn truncate(path: &str, size: u64) -> Result<(), FsError> {
    let fd = util::open("/data", path, OpenFlags::empty(), DescriptorFlags::READ | DescriptorFlags::WRITE).await?;
    fd.set_size(size).await
}

async fn ops(body: &str) -> String {
    for (i, line) in body.lines().enumerate() {
        let (op, expected) = line.split_once(" => ").expect("an expected result");
        let w: Vec<&str> = op.split(' ').collect();
        let n = |i: usize| w[i].parse::<u64>().expect("a number");
        let result = match w[0] {
            "write" => write(w[1], n(2), n(3) as usize, n(4) as u8).await,
            "truncate" => truncate(w[1], n(2)).await,
            "rename" => dir().rename_at(w[1].into(), dir(), w[2].into()).await,
            "unlink" => dir().unlink_file_at(w[1].into()).await,
            "mkdir" => dir().create_directory_at(w[1].into()).await,
            "rmdir" => dir().remove_directory_at(w[1].into()).await,
            other => panic!("unknown op {other}"),
        };
        let actual = kind(result);
        if actual != expected {
            return format!("mismatch {i} {actual}");
        }
    }
    "matched".into()
}

/// Exactly `bytes` more bytes and `files` more entries fit.
async fn probe(bytes: u64, files: u64) -> String {
    if files == 0 {
        let r = kind(write("~p0", 0, 0, 0).await);
        return if r == "quota" { "probed".into() } else { format!("probe: a new entry gave {r}, not quota") };
    }
    let mut written = 0;
    while written < bytes {
        let n = (bytes - written).min(256 << 10);
        let r = kind(write("~p0", written, n as usize, 1).await);
        if r != "ok" {
            return format!("probe: {r} after {written} of {bytes} bytes");
        }
        written += n;
    }
    let r = kind(write("~p0", bytes, 1, 1).await);
    if r != "insufficient-space" {
        return format!("probe: byte {bytes} past the room gave {r}");
    }
    for i in 1..=files {
        let r = kind(write(&format!("~p{i}"), 0, 0, 0).await);
        let expected = if i < files { "ok" } else { "quota" };
        if r != expected {
            return format!("probe: entry {} of {files} gave {r}", i + 1);
        }
    }
    "probed".into()
}
