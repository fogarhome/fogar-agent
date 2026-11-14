# VAD, wake word, inference runtimes, duty cycle and thread QoS

## VAD options (2026-11-12)

| Crate | Version / date | Engine | Frame | Notes |
|---|---|---|---|---|
| `earshot` | 1.2.2 (2026-08-19), MIT/Apache | pure-Rust NN, `no_std`+`libm` capable | **256 samples = 16 ms @16 kHz**, i16 or f32 | `Detector::default()`, `predict_f32(&[f32]) -> f32`, `predict_i16`, `predict(frame) -> Score` (`is_voice()`, `.raw`); ~8 KiB state, ~95 KiB binary [primary: README/src]. Vendor claims RTF 0.0003 and better accuracy than Silero v6/TEN [unverified]. Measured 0 allocs, RTF ≈ 0.0005–0.0006 here [exp]. **Default.** |
| `voice_activity_detector` | 0.2.1 (2025-08) | Silero VAD via `ort` **pinned `=2.0.0-rc.10`** | 512 @16k (Silero) | Version pin conflicts with a newer `ort` in the same graph [primary: Cargo.toml] |
| `webrtc-vad` | 0.4.0 (2019) | C WebRTC GMM VAD | 10/20/30 ms | Old, cheap, many false positives on noise [secondary] |
| openWakeWord's own Silero gate | — | Python only | — | oWW docs: VAD gating "can significantly reduce false-positive activations in the presence of non-speech noise" [primary: README] |

## Wake-word engines

| Option | Runtime | Status | Licence of models | Fit |
|---|---|---|---|---|
| openWakeWord model format (mel → embedding → classifier) | ONNX (ort or tract) or TFLite | Python reference, repo last commit 2025-12-30 | Code Apache-2.0; **pre-trained models CC BY-NC-SA 4.0** ("due to the inclusion of datasets with unknown or restrictive licensing"); embedding backbone derived from Google `speech_embedding` (Apache-2.0) [primary: README] | **Recommended format**; train a custom "fogar" model with the oWW training notebook; check licences of training data |
| `oww-rs` 0.3.3 (repo 0.3.4, 2026-10) | `tract-onnx 0.23` | small, MIT | bundles `alexa.onnx`, `hey_jarvis_v0.1.onnx`, `hey_mycroft`… (CC BY-NC-SA upstream) | Good **reference for the inference math**; do **not** copy its cpal callback (resamples, allocates `Vec`, logs `warn!` inside it) [primary: `mic_cpal.rs`] |
| `livekit-wakeword` 0.1.3 (2026-09) | `ort` with `alternative-backend` → `ort-tract` (pure Rust), native ORT on aarch64-windows-msvc | young | mel+embedding embedded in binary; classifiers trained with livekit-wakeword toolkit | Same 3-stage pipeline (32 mel bins, 96-d embeddings); `WakeWordModel::new(&[paths], sample_rate)`, `predict(&[i16])`; needs ~2 s of audio before non-zero scores [primary: README] |
| `rustpotter` 3.0.2 | MFCC + DTW "references" or candle 0.2 NN | **last release 2023-10**, deps candle 0.2.2 / rubato 0.14 | own | Few-shot personal wake word (3–8 recordings) is attractive, but unmaintained → only as experiment [primary: repo] |
| `sherpa-onnx` 2.0.1 (2026-10) | native sherpa-onnx C API | active | per-model | Open-vocabulary keyword spotting; heavier, native lib [secondary: crate description] |
| Picovoice Porcupine | proprietary | **all `pv_porcupine` versions on crates.io are yanked**; SDK needs an AccessKey/commercial licence | proprietary | Best-in-class on constrained HW per oWW's own comparison, but licence + key management [primary: crates.io API; oWW README] |
| microWakeWord (`micro-wakeword` 0.1.3) | TFLite | young | — | Designed for MCUs; oWW recommends it for ESP32-class targets [primary: oWW README] |

### openWakeWord pipeline facts [primary: `openwakeword/utils.py`, README]
- Input: **16-bit 16 kHz PCM**, "frames should be multiples of 80 ms" → 1280 samples per step.
- `melspectrogram.onnx`: input `[1, N]` samples, output 32 mel bins per 10 ms (observed `[1,1,5,32]`
  for 1280 samples [exp]; the Python streaming code adds 480 samples of context → 8 frames per step);
  transform `x/10 + 2` applied after.
- `embedding_model.onnx`: input `[1, 76, 32, 1]` (76 mel frames ≈ 775 ms), step 8 mel frames, output
  96-d (`[1,1,1,96]` [exp]).
- Classifier: input `[1, 16, 96]` (16 embeddings = 1.28 s), output score 0..1; default threshold 0.5,
  tune per environment. Target "<0.5 false accepts per hour, <5 % false rejects".
- Raspberry Pi 3: "a single core … can run 15-20 openWakeWord models simultaneously in real-time".
  The shared backbone means each extra wake word costs only the small classifier.

## Inference runtimes

| Runtime | Version | Pros | Cons |
|---|---|---|---|
| `tract-onnx` | 0.23.10 (2026-11-05) | pure Rust, no dylib, static binary, cross-compiles to Win/Linux trivially | Slower than ORT on some graphs [unverified for these models]; API churn (`to_plain_array_view`, `into_runnable()` → `Arc<…>`) [exp] |
| `ort` | **2.0.0-rc.15** (still RC, 2026-10-19) | ONNX Runtime speed; CoreML/XNNPACK EPs; `alternative-backend` lets the same API run on `ort-tract 0.5.0+0.23` or `ort-candle 0.5.0+0.11.0` (call `ort::set_api` first) | default features include `download-binaries` (fetches prebuilt ORT at build time — supply chain), `tracing`, `copy-dylibs`; adds MBs of native code; intra-op threads **spin by default** [primary: Cargo.toml + `impl_config_keys.rs`] |
| `candle-core`/`candle-onnx` | 0.11.1 | pure Rust, Metal backend | ONNX op coverage partial [unverified for oWW graphs] |

ORT settings that matter for energy (all names present in rc.15 source [primary]; note rc.15's
`Session::builder` now takes an `&Environment` — older blog posts show `Session::builder()`):
```rust
let env = ort::init().with_telemetry(false).build()?;   // EnvironmentBuilder::{with_telemetry, build}
let session = ort::session::Session::builder(&env)?
    .with_intra_threads(1)?            // tiny models: parallelism costs more than it saves
    .with_inter_threads(1)?
    .with_intra_op_spinning(false)?    // "session.intra_op.allow_spinning" — default ENABLED
    .with_inter_op_spinning(false)?    // default ENABLED
    .commit_from_file(path)?;
```
(`GlobalThreadPoolOptions::with_spin_control(false)` exists for a shared pool.) Not compiled here
(the build downloads ORT binaries) — compile-check on the Mac.
CoreML EP for models this small: dispatch overhead likely dominates and ANE/GPU wake-ups cost
energy [unverified] — measure CPU EP vs CoreML on the M2 before enabling.

### Measured cost (Linux x86 Xeon 2.1 GHz; tract 0.23.10; single-threaded) [exp]
| Stage | RTF | ≈ % of one core | Allocs |
|---|---|---|---|
| earshot VAD | 0.0005–0.0006 | 0.05–0.06 % | 0 |
| oWW mel + embedding + 1 classifier, per 80 ms | 0.032–0.036 | 3.2–3.6 % | ~349 per frame (tensor creation + tract internals) |
| rubato Fft 48k→16k | 0.00044–0.00046 | 0.045 % | 0 |
| capture-side downmix + rtrb | 0.00005 | 0.005 % | 0 |
Max score on a synthetic tone: 0.0012 (sanity check only; not an accuracy test).
Apple M2 E-cores are much slower than P-cores; numbers here say nothing about M2 energy.

## Duty cycle: gate inference with VAD (recommended design, measure FRR impact)
1. Always run: capture → resample → earshot every 16 ms (≈0.06 % core here).
2. Keep a 2 s pre-roll of 16 kHz audio (32 000 f32 = 128 KB).
3. When VAD smoothed score ≥ attack threshold (earshot's segmenter defaults: attack 0.6, decay 0.5
   [primary: `segments.rs`]), start wake-word inference: first catch up over the last ~1.3–2 s of
   pre-roll (to fill the 16-embedding context), then run every 80 ms.
4. Stop inference after VAD hangover (e.g. 1 s of non-voice).
In a quiet room this skips ~all of the 55× more expensive oWW work. Risk: VAD misses the start of a
whispered/far-field wake word → measure false-reject rate with gating on vs off.
Simpler alternative if FRR suffers: run mel every 80 ms always (cheap part) and gate only the
embedding + classifier.

## Threads and QoS (macOS untested)
| Thread | Who creates | Priority | Notes |
|---|---|---|---|
| CoreAudio IO thread | the HAL | real-time (time-constraint) [secondary] | Our callback runs here; never promote anything else |
| audio-ctl (stream owner) | adapter | default | Mostly parked on a channel |
| dsp (resample/VAD/wake/Opus) | core | **QoS utility** | `unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0) }` at thread start (symbol and enum present in libc 0.2.191 [primary]); Apple: utility = "balance between responsiveness, performance, and energy efficiency"; background "emphasizes energy efficiency" and is "more likely to run … on lower performance cores" [primary: Apple Silicon tuning guide] |
| tokio runtime (network) | app | default/utility | small (1–2 workers) |
- Try `QOS_CLASS_BACKGROUND` for the dsp thread only if `frames_dropped` stays 0 and wake latency is
  acceptable — background work can be heavily deferred [secondary].
- Apple asks to replace `pthread_setschedparam`/`setpriority` with QoS APIs [primary].
- `audio_thread_priority` (mozilla, 0.38.0) is for promoting *your own* RT threads (Windows MMCSS,
  Linux rtkit, macOS time-constraint); fogar doesn't need it because it never renders audio.
