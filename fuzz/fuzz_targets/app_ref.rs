#![no_main]
// See nanovm::fuzz::app_ref (fuzz/entry.rs) for the properties checked.
libfuzzer_sys::fuzz_target!(|data: &[u8]| nanovm::fuzz::app_ref(data));
