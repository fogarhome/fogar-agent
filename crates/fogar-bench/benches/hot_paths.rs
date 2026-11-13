//! PLACEHOLDER benchmark harness (std only; `cargo bench -p fogar-bench`). Measures per-call cost of
//! the per-frame hot paths so regressions show up early. NOT the energy gate: idle CPU, wakeups/s
//! and phys_footprint are measured on the M2 with the fogar-energy-budget protocol (S7).
#![allow(clippy::unwrap_used, clippy::disallowed_methods)]

use std::{hint::black_box, time::Instant};

fn bench(name: &str, iters: u32, mut f: impl FnMut()) {
    for _ in 0..iters / 10 {
        f();
    }
    let t = Instant::now();
    for _ in 0..iters {
        f();
    }
    let per = t.elapsed() / iters;
    println!("{name:<40} {per:>12?}/iter ({iters} iters)");
}

fn main() {
    let mut tts = Vec::new();
    fogar_proto::encode_tts_frame(1, &[123i16; 4800], &mut tts);
    bench("parse_tts_frame (200 ms frame)", 100_000, || {
        let f = fogar_proto::parse_tts_frame(black_box(&tts)).unwrap();
        black_box(f.samples().fold(0i32, |a, s| a.wrapping_add(i32::from(s))));
    });
    let samples = vec![0.25f32; 320];
    let mut out = Vec::with_capacity(320 * 4);
    bench("encode_upload_f32le (20 ms chunk)", 100_000, || {
        out.clear();
        fogar_proto::encode_upload_f32le(black_box(&samples), &mut out);
    });
    let token = r#"{"type":"token","turn_id":"t-1","text":"Vale, pongo la serie."}"#;
    bench("decode_server(token)", 100_000, || {
        black_box(fogar_proto::decode_server(black_box(token)).unwrap());
    });
    let mut vad = fogar_core::vad::EnergyVad::new(fogar_core::vad::VadConfig::default());
    bench("EnergyVad::process (20 ms chunk)", 100_000, || {
        black_box(vad.process(black_box(&samples)));
    });
}
