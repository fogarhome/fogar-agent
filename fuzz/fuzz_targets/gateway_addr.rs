#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| fogar_fuzz::gateway_addr(data));
