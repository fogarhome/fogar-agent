#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| fogar_fuzz::close_code(data));
