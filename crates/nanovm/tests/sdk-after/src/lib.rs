//! Test-only app on nanovm-sdk: work after the answer (`after`) and how it
//! fails. Never published.
use std::cell::Cell;
use std::time::Duration;

use nanovm_sdk::{Request, Response, Result, after, data};

nanovm_sdk::serve!(handle);

thread_local! {
    /// Requests this instance has served: 1 on a fresh one.
    static SERVED: Cell<u32> = const { Cell::new(0) };
}

const FILES: [&str; 4] = ["answered", "later", "nested", "streamed"];

async fn mark(name: &str) {
    data().write(name, "1").await.unwrap();
}

async fn handle(req: Request) -> Result {
    SERVED.set(SERVED.get() + 1);
    if req.path() == "/files" {
        let mut out = format!("{{\"served\":{}", SERVED.get());
        for name in FILES {
            out += &format!(",\"{name}\":{}", data().read(name).await?.is_some());
        }
        return Ok(Response::json(out + "}"));
    }
    mark("answered").await;
    match req.path() {
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
            return Ok(Response::text("x".repeat(1 << 20)));
        }
        _ => return Ok(Response::not_found()),
    }
    Ok(Response::text("answered"))
}
