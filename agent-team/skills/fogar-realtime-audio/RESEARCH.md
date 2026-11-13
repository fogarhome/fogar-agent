# RESEARCH — fogar-realtime-audio (2026-11-12)

Method: crate versions from the crates.io API; source read from shallow clones (each in its own
directory, read-only, nothing executed from them); Apple docs via developer.apple.com `.md`
endpoints; Apple DTS answers from developer forums; small pure-Rust experiments written from scratch
and run in a Linux x86 container (rustc 1.97.0). The `aarch64-apple-darwin` std could not be
downloaded (proxy), so **no macOS compile or run** happened. ONNX model files from the oww-rs repo
were used as data for a timing benchmark only.

Labels: [primary] = source code, official docs/headers, maintainer text; [exp] = measured here;
[secondary] = forums, third-party docs, other projects' bugs; [unverified] = memory/inference.

## 1. Sources

### Crates (versions from https://crates.io/api/v1/crates/<name>, fetched 2026-11-12)
| Crate | Version (date) | Repo / what was read |
|---|---|---|
| cpal | 0.18.2 (2026-08-16); master 0.19.0 unreleased @40ff3ad 2026-11-11 | https://github.com/RustAudio/cpal — `CHANGELOG.md`, `src/traits.rs`, `src/lib.rs`, `src/error.rs`, `src/host/coreaudio/macos/{mod,device}.rs`, `src/host/error_emit.rs`, `src/host/wasapi/{device,stream}.rs`, `Cargo.toml` |
| rtrb | 0.4.1 (2026-10-25) | https://github.com/mgeier/rtrb — `src/lib.rs`, `src/chunks.rs` |
| ringbuf | 0.6.0 (2026-10-30) | https://github.com/agerasev/ringbuf — README, traits, `async/src/rb.rs` |
| rubato | 5.0.1 (2026-10-01) | https://github.com/HEnquist/rubato — README, `src/synchro.rs`, `src/asynchro.rs`, `src/lib.rs` |
| audioadapter / -buffers | 5.0.0 / 5.1 | via rubato README; used in experiment |
| opus | 0.4.0 (2026-08-23) | https://github.com/SpaceManiac/opus-rs — `src/lib.rs`, `Cargo.toml` |
| opusic-sys | 0.7.7 (2026-11-04), libopus 1.6.1 | crate tarball — README, `Cargo.toml`, `opus/include/opus.h`, `opus_defines.h`, `package_version` |
| opusic-c, audiopus, unsafe-libopus, mousiki, ropus | see opus-encoding.md | crates.io metadata only |
| ort | 2.0.0-rc.15 (2026-10-19) | https://github.com/pykeio/ort tag v2.0.0-rc.15 — `Cargo.toml`, `src/session/builder/*`, `src/session/mod.rs`, `src/environment.rs`, `src/lib.rs`, `src/ep/coreml.rs` |
| ort-tract / ort-candle | 0.5.0+0.23 / 0.5.0+0.11.0 | crates.io metadata |
| tract-onnx | 0.23.10 (2026-11-05) | crate tarball `src/lib.rs`; used in experiment |
| candle-core / candle-onnx | 0.11.1 | metadata only |
| earshot | 1.2.2 (2026-08-19) | https://github.com/pykeio/earshot — README, `src/lib.rs`, `src/segments.rs`; used in experiment |
| voice_activity_detector | 0.2.1 (2025-08-04) | https://github.com/nkeenan38/voice_activity_detector — `Cargo.toml` |
| webrtc-vad | 0.4.0 (2019) | metadata |
| openWakeWord (Python) | repo HEAD 2025-12-30 | https://github.com/dscripka/openWakeWord — README, `openwakeword/utils.py` |
| oww-rs | 0.3.3 (repo 0.3.4, 2026-10-16) | https://github.com/skoky/oww_rs — README, Cargo.toml, `crates/oww/src/mic_cpal.rs`, `oww/oww_model.rs` |
| livekit-wakeword | 0.1.3 (2026-09-11) | crate tarball README/Cargo.toml (repo https://github.com/livekit/rust-sdks) |
| rustpotter | 3.0.2 (2023-10-01) | https://github.com/GiviMAD/rustpotter — README, Cargo.toml |
| sherpa-onnx | 2.0.1 (2026-10-27) | metadata |
| pv_porcupine | all versions yanked | crates.io API `versions[].yanked` |
| micro-wakeword | 0.1.3 | metadata |
| tokio-tungstenite / tungstenite | 0.30.0 / 0.30.0 | tungstenite tarball `src/protocol/mod.rs`, `src/protocol/message.rs`; feature list via API |
| loom | 0.7.3 (2026-11-08) | registry source `src/lib.rs`, `src/rt/{atomic,synchronize}.rs`; used in experiment |
| assert_no_alloc | 1.1.2 (2021) | registry source `src/lib.rs`, `Cargo.toml` |
| rtsan-standalone | 0.3.0 (2025-09-27) | https://github.com/realtime-sanitizer/rtsan-standalone-rs README |
| libc | 0.2.191 | tarball `src/new/apple/libpthread/{sys,pthread_}/qos.rs` |
| objc2-core-audio / -av-foundation / -io-kit / -service-management | 0.3.2 (2025-10-04) | tarballs, generated bindings incl. Apple header doc text |
| audio_thread_priority | 0.38.0 | metadata + use in cpal wasapi |
| basedrop, triple_buffer, crossbeam-queue, heapless | see ring-buffers-and-loom.md | metadata |

### Apple / Microsoft / standards
- Apple, "Requesting authorization to capture and save media" — https://developer.apple.com/tutorials/data/documentation/avfoundation/requesting-authorization-to-capture-and-save-media.md
- Apple, "Protected resources" — https://developer.apple.com/documentation/bundleresources/protected-resources.md
- Apple, "Hardened Runtime" — https://developer.apple.com/tutorials/data/documentation/security/hardened-runtime.md
- Apple, "Tuning your code's performance for Apple silicon" — https://developer.apple.com/tutorials/data/documentation/apple-silicon/tuning-your-code-s-performance-for-apple-silicon.md
- Apple Support, Control Centre (orange dot) — https://support.apple.com/en-nz/guide/mac-help/mchl50f94f8f/mac
- Apple DTS forum threads: https://developer.apple.com/forums/thread/701094 (responsible code, 2022),
  https://developer.apple.com/forums/thread/853725 (daemon TCC, 2026),
  https://developer.apple.com/forums/thread/741303 (hardened runtime mic, 2023),
  https://developer.apple.com/forums/thread/835256 (macOS 26.5.1 mic, audio-input entitlement)
- AVCaptureDevice / IOKit pwr_mgt / SMAppService header text as reproduced in objc2 generated bindings (0.3.2)
- Rust std `Instant` — https://doc.rust-lang.org/std/time/struct.Instant.html
- Microsoft Support, microphone app permissions — https://support.microsoft.com/en-US/Windows/privacy/turn-on-app-permissions-for-your-microphone-in-windows
- Mozilla Bugzilla 1964935 (macOS output rate drops during mic use) — https://bugzilla.mozilla.org/show_bug.cgi?id=1964935
- NOT fetched (blocked): RFC 6716 (rfc-editor.org, datatracker), developer.apple.com regular HTML pages,
  docs.rs. Bitrate "sweet spots" therefore [unverified].

## 2. Experiments (code in the research scratchpad, not shipped with the skill)
1. **loom SPSC** (`experiments/spsc_loom`): correct Release/Acquire ring passes (42 iterations);
   Relaxed-publish bug caught ("Causality violation: Concurrent read and write accesses") **only after
   adding `loom::thread::yield_now()` to the consumer poll loop; without it loom ran 1 iteration and the
   bug passed** [exp].
2. **Port compile check** (`experiments/port_check`): core port + cpal adapter compile against cpal
   =0.18.2 / rtrb 0.4.1 on Linux (libasound2-dev installed). Confirms: `build_input_stream(cfg_by_value,
   data_cb(&[T], &cpal::InputCallbackInfo), err_cb(cpal::Error), Option<Duration>)`, `e.kind()`,
   `ErrorKind::{DeviceNotAvailable, StreamInvalidated, DeviceChanged, Xrun, PermissionDenied, DeviceBusy,
   UnsupportedConfig}`, `sample_rate: u32`, `SupportedBufferSize::Range{min,max}` by reference,
   `stream.play()`. RT-path test: 0 allocations, exact drop accounting [exp].
3. **rt_bench** (`experiments/rt_bench`, release, counting allocator, 60 s synthetic audio, 3 runs):
   downmix+rtrb 0.005 % core, 0 allocs; rubato Fft 48k→16k chunk 960 0.044–0.046 %, 0 allocs
   (output 320/321 frames per call, delay 53); earshot 0.053–0.065 %, 0 allocs; Opus 16k mono 20 ms:
   24 kb/s cplx10 0.30–0.31 % (61 B/pkt), cplx5 0.52–0.54 %, 16 kb/s cplx5 0.51–0.55 % (40 B/pkt),
   0 allocs; openWakeWord mel+embedding+hey_jarvis via tract 3.2–3.6 %, ~349 allocs per 80 ms frame.
   Machine: Intel Xeon @2.1 GHz, 2 vCPU, Linux 6.18. **Not representative of the M2.**
4. tract 0.23 API drift found by compiling: `TValue::to_array_view` → `to_plain_array_view`;
   `into_runnable()` returns `Arc<SimplePlan<…>>`; `TypedRunnableModel` takes no generic [exp].

## 3. Claims and confidence

| # | Claim | Label | Evidence |
|---|---|---|---|
| 1 | cpal 0.18.2 macOS input streams do not follow default-input changes | primary | device.rs comment "Input is not automatically rerouted"; only output uses DefaultOutput unit + `DefaultOutputMonitor` |
| 2 | cpal error callback can be invoked from the CoreAudio RT thread, via `Mutex::try_lock` | primary | mod.rs "Overload notifications fire on the RT thread"; error_emit.rs |
| 3 | cpal pauses the stream on DeviceIsAlive / nominal-rate change and reports DeviceNotAvailable / StreamInvalidated | primary | mod.rs DisconnectManager |
| 4 | cpal sets the device's physical format/nominal rate to the requested config | primary | device.rs `set_physical_format` / `set_sample_rate` |
| 5 | macOS default input config only F32/I16 in 0.18.2 | primary | device.rs error string |
| 6 | Streams don't auto-start since 0.18 (`play()` required) | primary | CHANGELOG 0.18.0 |
| 7 | `BufferSize::Fixed` is a request; size may vary | primary | lib.rs docs |
| 8 | Buffer frame size is a device-level property | primary (cpal comment), HAL multi-client semantics unverified |
| 9 | rtrb is wait-free, no blocking wait, no overwrite | primary | lib.rs docs |
| 10 | async-ringbuf wakes an AtomicWaker on push | primary | async/src/rb.rs |
| 11 | rubato `process_into_buffer` doesn't allocate; `log` feature allocates | primary + exp |
| 12 | assert_no_alloc is a no-op in release by default | primary | Cargo.toml default `disable_release` |
| 13 | RTSan supports Linux, macOS, iOS | primary | README |
| 14 | loom misses bugs without yield points in poll loops (this scenario) | exp |
| 15 | openWakeWord: 16 kHz int16, 80 ms frames, embedding [1,76,32,1]→96, classifier 16×96, threshold 0.5 | primary | utils.py, README; shapes confirmed by tract [exp] |
| 16 | openWakeWord pre-trained models are CC BY-NC-SA 4.0 | primary | README License |
| 17 | oww-rs callback allocates/logs/resamples | primary | mic_cpal.rs |
| 18 | ORT intra/inter-op spinning enabled by default | primary | ort impl_config_keys.rs doc comments |
| 19 | ort rc.15 `Session::builder(&Environment)` | primary | session/mod.rs:159 |
| 20 | ort default features download binaries at build | primary | Cargo.toml |
| 21 | libopus allowed rates 8/12/16/24/48 kHz; frames 2.5–60 ms; <10 ms disables LPC/hybrid; DTX off by default | primary | opus.h / opus_defines.h 1.6.1 |
| 22 | RFC 6716 bitrate sweet spots | unverified | memory; RFC not fetchable |
| 23 | tungstenite read buffer 128 KiB eager; write buffer 128 KiB; max_write_buffer not backpressure | primary | protocol/mod.rs docs |
| 24 | Mic needs NSMicrophoneUsageDescription + audio-input entitlement; else terminated | primary | Apple article |
| 25 | Hardened-runtime apps without audio-input get silent denial | secondary | DTS forum 835256 |
| 26 | TCC edge cases for launchd agents undocumented; daemons can't inherit app grants | secondary | DTS forum 853725 |
| 27 | Ungranted AVCaptureDevices vend silent samples | primary (AVFoundation); extension to AUHAL/cpal unverified |
| 28 | Orange menu-bar dot while mic in use | primary | Apple Support |
| 29 | Apple: use QoS via `pthread_set_qos_class_self_np`; avoid spin-waits and `pthread_yield_np`; daemons should set `ProcessType` | primary | Apple Silicon tuning guide |
| 30 | `libc::pthread_set_qos_class_self_np` + `qos_class_t::QOS_CLASS_UTILITY` exist | primary | libc 0.2.191 source |
| 31 | Instant suspend accounting unspecified | primary | std docs |
| 32 | IOKit sleep/wake messages and ack rules | primary | header text via objc2-io-kit |
| 33 | Bluetooth headset mic use degrades output (profile/rate) | secondary | Mozilla bug 1964935 (with voice processing), user forums |
| 34 | CoreAudio HAL IO thread is real-time | secondary | general CoreAudio knowledge, not re-fetched |
| 35 | Per-stage CPU numbers | exp (x86 only) |
| 36 | Windows desktop-app mic privacy toggle exists, per-app settings | primary | Microsoft Support |
| 37 | Windows/Linux power-event API names, Session-0 limitation | unverified |

## 4. Conflicts recorded
- **cpal WASAPI default-device behaviour**: 0.18.0 changelog — "Default output and input streams now
  automatically reroute when the system default device changes; … receives DeviceChanged". 0.18.2 code —
  "WASAPI never rebinds the IAudioClient, so report what's actually true instead of DeviceChanged"
  (StreamInvalidated/DeviceNotAvailable). master — "Revert 'Default device changes no longer report
  DeviceChanged' as it was misinformed". → Treat every signal as "rebuild"; test on Windows.
- **cpal `ErrorKind::Xrun`**: added for CoreAudio in 0.18.2, removed on master in favour of
  `CallbackInfo::xrun()`.
- **ProcessType**: sibling skill `fogar-macos-daemon` recommends `Background`; Apple warns Background
  imposes resource limits; for audio, unknown effect → measure (`frames_dropped`).
- **earshot vs Silero accuracy**: only the vendor's claim (pyke.io), no independent benchmark found.
- **Opus complexity 5 slower than 10** on a synthetic tone here — contradicts expectation; signal-dependent.

## 5. Open questions
1. Does a LaunchAgent-started `LSUIElement` bundle get a visible TCC prompt? Which identity is shown?
2. Revoked/ungranted mic on AUHAL: zeros, error, or stream failure? Does cpal surface `PermissionDenied`?
3. HAL buffer-size arbitration between clients; actual callback size delivered on the M2 built-in mic.
4. Energy of an open built-in mic stream on M2 (package power delta, wakeups/s).
5. Does `ProcessType=Background` throttle the dsp thread enough to drop audio?
6. Best runtime for oWW-format models on M2 E-cores: tract vs ORT CPU vs ORT CoreML (energy, not just ms).
7. False-reject impact of VAD gating with pre-roll catch-up.
8. Behaviour across sleep/wake: does the AUHAL stream resume by itself? Device IDs stable for USB/BT?
9. Is `cpal::Host` / adapter `Send` on macOS as assumed by `AudioSource: Send`?
10. Licence path for a commercial "fogar" wake word (training data licences of oWW pipeline).

## 6. Must measure on the real M2 (summary; plan in references/power-and-measurement.md)
Idle CPU and wakeups/s with mic open (silence) vs closed · energy impact / package power · RSS at 72 h ·
callback size & period · frames_dropped under CPU load and with ProcessType variants · oWW cost per
runtime · FA/h and FRR with/without gating · rebuild latency on device switch, BT connect, USB unplug,
sleep/wake · TCC prompt attribution and revoke behaviour · AirPods output-quality impact · compile of
the port code for aarch64-apple-darwin · RTSan run on the real callback.
