//! Test-only app on nanovm-sdk: work after the answer (`after`, a streamed
//! body) and how it fails. Never published.
use std::cell::Cell;
use std::time::Duration;

use nanovm_sdk::http::{Request, Response};
use nanovm_sdk::{Body, Error, Result, after, data};

nanovm_sdk::serve!(handle);

thread_local! {
    /// Requests this instance has served: 1 on a fresh one.
    static SERVED: Cell<u32> = const { Cell::new(0) };
}

const FILES: [&str; 4] = ["answered", "later", "nested", "streamed"];

async fn mark(name: &str) {
    data().write(name, "1").await.unwrap();
}

async fn handle(req: Request<Body>) -> Result {
    SERVED.set(SERVED.get() + 1);
    if req.uri().path() == "/files" {
        let mut out = format!("{{\"served\":{}", SERVED.get());
        for name in FILES {
            out += &format!(",\"{name}\":{}", data().read(name).await?.is_some());
        }
        return Ok(Response::new((out + "}").into()));
    }
    mark("answered").await;
    match req.uri().path() {
        // Work that panics before it first waits.
        "/panic-at-once" => after(async { panic!("after work failed at once") }),
        // Work that writes, then panics.
        "/panic-later" => after(async {
            mark("later").await;
            panic!("after work failed");
        }),
        // Work that passes on more work, which writes.
        "/nested" => after(async {
            nanovm_sdk::sleep(Duration::from_millis(20)).await;
            after(async { mark("nested").await });
        }),
        // Work that passes on more work, which writes, then panics.
        "/nested-panic" => after(async {
            after(async {
                mark("nested").await;
                panic!("nested after work failed");
            });
        }),
        // A body of several pieces, then work that fails at once.
        "/big-panic" => {
            after(async { panic!("after work failed at once") });
            return Ok(Response::new("x".repeat(1 << 20).into()));
        }
        // A streamed body that changes something after the answer, then
        // fails. The node takes a first piece as it takes the answer, a
        // second only once the client has read the first: after the
        // answer's commit.
        "/cut" => {
            return Ok(Response::new(Body::stream(|mut body| async move {
                body.write("part").await;
                body.write("ial").await;
                mark("streamed").await;
                Err(Error::new("the stream broke off"))
            })));
        }
        _ => return Ok(Response::builder().status(404).body(Body::empty())?),
    }
    Ok(Response::new("answered".into()))
}
