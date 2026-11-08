#![no_main]
// See nanovm::fuzz::grip_publish (fuzz/entry.rs) for the properties checked.
libfuzzer_sys::fuzz_target!(|data: &[u8]| nanovm::fuzz::grip_publish(data));
