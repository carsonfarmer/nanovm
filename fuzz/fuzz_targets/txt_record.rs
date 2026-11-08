#![no_main]
// See nanovm::fuzz::txt_record (fuzz/entry.rs) for the properties checked.
libfuzzer_sys::fuzz_target!(|data: &[u8]| nanovm::fuzz::txt_record(data));
