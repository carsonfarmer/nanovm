#![no_main]
// See nanovm::fuzz::ws_events (fuzz/entry.rs) for the properties checked.
libfuzzer_sys::fuzz_target!(|data: &[u8]| nanovm::fuzz::ws_events(data));
