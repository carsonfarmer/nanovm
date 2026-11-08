#![no_main]
// See nanovm::fuzz::telemetry_stamp (fuzz/entry.rs) for the properties checked.
libfuzzer_sys::fuzz_target!(|data: &[u8]| nanovm::fuzz::telemetry_stamp(data));
