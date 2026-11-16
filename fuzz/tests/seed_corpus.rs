//! Stable smoke check: every target body runs over its committed seeds (no nightly needed).

use std::fs;
use std::path::Path;

fn run(target: &str, f: fn(&[u8])) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("corpus")
        .join(target);
    let mut n = 0;
    for entry in fs::read_dir(&dir).unwrap() {
        f(&fs::read(entry.unwrap().path()).unwrap());
        n += 1;
    }
    assert!(n > 0, "no seeds in {}", dir.display());
}

#[test]
fn decode_server() {
    run("decode_server", fogar_fuzz::decode_server);
}

#[test]
fn tts_frame() {
    run("tts_frame", fogar_fuzz::tts_frame);
}

#[test]
fn close_code() {
    run("close_code", fogar_fuzz::close_code);
}

#[test]
fn ipc_frames() {
    run("ipc_frames", fogar_fuzz::ipc_frames);
}

#[test]
fn gateway_addr() {
    run("gateway_addr", fogar_fuzz::gateway_addr);
}
