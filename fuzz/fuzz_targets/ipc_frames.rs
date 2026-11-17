#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| fogar_fuzz::ipc_frames(data));
