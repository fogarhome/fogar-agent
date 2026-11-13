# Opus encoding for the voice uplink

## Crate options (crates.io, 2026-11-12)

| Crate | Version | What | Verdict |
|---|---|---|---|
| `opus` | 0.4.0 (2026-08-23) | Safe bindings; now depends on `opusic-sys 0.7.3+` | **Default.** `Encoder::new(sr, Channels, Application)`, `encode`/`encode_float(&[f32], &mut [u8]) -> Result<usize>`, setters `set_bitrate(Bitrate::Bits(i32))`, `set_complexity`, `set_signal`, `set_inband_fec`, `set_packet_loss_perc`, `set_dtx`, `set_bandwidth`, `set_max_bandwidth`; `unsafe impl Send for Encoder` [primary: src] |
| `opusic-sys` | 0.7.7 (2026-11-04) | Raw FFI; feature `bundled` (default) builds **libopus 1.6.1** with CMake; features `dred`, `osce`, `fixed-point`, `no-simd`… | Transitively used. Build needs `cmake` (+ optional `ninja`); `OPUS_LIB_DIR`/`OPUS_LIB_STATIC` for system lib [primary: README, package_version] |
| `opusic-c` | 1.6.1 | Higher-level bindings by the opusic-sys author | Alternative [primary: crates.io metadata] |
| `audiopus` | 0.2.0 (2019; 0.3.0-rc.0 2021) | Old bindings | Stale — avoid |
| `unsafe-libopus` | 0.2.0 (2024) | c2rust transpile, pure Rust | Interesting for no-C builds; low adoption [secondary] |
| `mousiki` 0.2.1, `ropus` 0.12.18 | pure-Rust ports | Young (≈4k / 14k downloads) | Watch, don't ship yet [unverified quality] |

## Codec facts [primary: libopus 1.6.1 `include/opus.h`]
- Sampling rates for `opus_encoder_create`: **8000, 12000, 16000, 24000, 48000**; 1 or 2 channels.
- Frame sizes "from 2.5 ms to 60 ms"; at 48 kHz permitted values 120, 240, 480, 960, 1920, 2880.
  At 16 kHz: 40, 80, 160, **320 (20 ms)**, 640, 960. "Passing in a duration of less than 10 ms … will
  prevent the encoder from using the LPC or hybrid modes" → use ≥10 ms for speech; **20 ms** is the
  standard choice.
- `OPUS_APPLICATION_VOIP`: "best quality at a given bitrate for voice signals… high-pass filtering and
  emphasizing formants and harmonics. Optionally … in-band forward error correction".
- `max_data_bytes` "should not be used as the only bitrate control. Use OPUS_SET_BITRATE".
- DTX default **disabled** (`OPUS_GET_DTX`: "0 DTX disabled (default)").
- Bit-rates 6–510 kb/s.

## Recommended encoder config for fogar (speech to a server STT)
```rust
let mut enc = opus::Encoder::new(16000, opus::Channels::Mono, opus::Application::Voip)?;
enc.set_bitrate(opus::Bitrate::Bits(24_000))?;   // wideband speech; 16 kb/s also fine
enc.set_complexity(5)?;                           // measure 5 vs 10 on the M2
enc.set_signal(opus::Signal::Voice)?;             // hint
enc.set_inband_fec(false)?;                       // TCP/WebSocket: no packet loss to protect against
enc.set_dtx(false)?;                              // keep constant cadence; DTX saves bytes in silence
let mut pkt = [0u8; 1500];                        // reuse; never allocate per frame
let n = enc.encode_float(&pcm_16k_20ms /* 320 samples */, &mut pkt)?;
```
- Bitrate guidance: RFC 6716 §2.1.1 lists "sweet spots" of roughly 8–12 kb/s NB speech, 16–20 kb/s
  WB speech, 28–40 kb/s FB speech [unverified: quoted from memory; the RFC page could not be fetched
  in this environment]. For ASR, 16–24 kb/s wideband at 16 kHz is the pragmatic range [inference];
  validate WER vs bitrate with the actual STT provider.
- Feed 16 kHz directly (no need to upsample to 48 kHz); the header notes the encoder can always
  lower bandwidth itself, so 48 kHz input is also legal.
- FEC/`packet_loss_perc` only help on lossy transports (UDP/WebRTC). Over WebSocket (TCP) losses are
  retransmitted → disable FEC, it wastes bits.
- DTX: worthwhile only if streams stay open during long silence; after wake the utterance is short
  and the server VAD/endpointing expects continuous frames → off.

## Measurements (Linux x86 Xeon @2.1 GHz, 60 s synthetic tone — NOT speech, NOT the M2) [exp]
| Config | RTF | ≈ % of one core | Avg packet | Allocs per encode |
|---|---|---|---|---|
| 16k mono 20 ms, 24 kb/s, complexity 10 | 0.0030 | 0.30 % | 61 B (24.4 kb/s) | 0 |
| 16k mono 20 ms, 24 kb/s, complexity 5 | 0.0052 | 0.52 % | 59 B | 0 |
| 16k mono 20 ms, 16 kb/s, complexity 5 | 0.0055 | 0.55 % | 40 B (15.8 kb/s) | 0 |
Complexity 5 being *slower* than 10 on a synthetic tone is counter-intuitive and reproducible here;
it is likely signal-dependent mode selection — **re-measure with real speech on the M2**.
(The "1 alloc" in the raw benchmark output was the `format!` of the label, not the encoder.)

## Packetization for the uplink
- One WebSocket binary message per 20 ms packet is simplest (WS preserves message boundaries).
  To cut per-message overhead/wake-ups, batch 2–5 packets (40–100 ms) per message with a tiny header:
  `u8 version | u8 count | u32 seq | u64 capture_ts_us | (u16 len, bytes)*count`.
- Alternatively Ogg Opus pages if the server wants a file-like stream (needs granule positions and
  OpusHead/OpusTags — more work; only if the server requires it).
- Send a stream header first (codec, sample rate, channels, frame duration, encoder lookahead
  `OPUS_GET_LOOKAHEAD`) so the server can decode without guessing.
- Encoding runs on the dsp thread (not RT, not tokio). The encoder is `Send` (no `Sync` impl): keep one
  per stream and call `enc.reset_state()?` between utterances; `enc.get_lookahead()?` gives the
  lookahead for the stream header [primary: opus 0.4.0 src].
