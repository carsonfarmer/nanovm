#![no_main]
// See nanovm::fuzz::journal_log (fuzz/entry.rs) for the properties checked.
libfuzzer_sys::fuzz_target!(|data: &[u8]| nanovm::fuzz::journal_log(data));
