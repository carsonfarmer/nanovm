//! A command (`wasi:cli/run`) instead of a service: it imports nothing
//! nanovm refuses but has no `wasi:http/handler` to call.
use util::wasip3;

wasip3::cli::command::export!(Command);
struct Command;

impl wasip3::exports::cli::run::Guest for Command {
    async fn run() -> Result<(), ()> {
        Ok(())
    }
}
