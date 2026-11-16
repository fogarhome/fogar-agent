#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| fogar_fuzz::decode_server(data));
