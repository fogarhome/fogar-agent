---
name: fogar-realtime-audio
description: >-
  Real-time microphone capture, wake word, VAD, resampling, Opus encoding and audio streaming for
  fogar-agent, the always-on headless Rust daemon (macOS M2 first, Windows/Linux later, hexagonal
  ports & adapters). Use when writing or reviewing the AudioSource port or its cpal/CoreAudio,
  WASAPI, ALSA/PulseAudio/PipeWire adapters; audio callback code (no alloc/locks/logging/syscalls);
  rtrb/ringbuf SPSC ring buffers and loom tests; rubato resampling 48k->16k and format negotiation;
  opus/libopus encoding (frame size, bitrate, DTX, FEC); earshot/Silero/webrtc VAD; openWakeWord,
  oww-rs, livekit-wakeword, rustpotter, sherpa-onnx KWS, Porcupine; ort/tract/candle inference and
  CPU/energy duty cycles; thread QoS (pthread_set_qos_class_self_np) vs real-time audio threads;
  default-input-device switching, sleep/wake, Bluetooth/AirPods route changes, xruns, stalls;
  microphone TCC permission (NSMicrophoneUsageDescription, com.apple.security.device.audio-input)
  for a LaunchAgent; orange mic indicator; streaming Opus over WebSocket (tokio-tungstenite) with
  backpressure; measuring mic power cost, wakeups and footprint on the MacBook Air M2.
---

# fogar-realtime-audio

Labels: **[primary]** source/docs/headers · **[exp]** measured in a Linux x86 container, NOT the M2 ·
**[secondary]** · **[unverified]**. Everything macOS-specific is **untested**. Versions checked
2026-11-12. Evidence, conflicts, open questions: `RESEARCH.md`.

## 1. Shape to build

```
OS audio thread (RT, owned by CoreAudio/WASAPI/PipeWire)
  └ cpal data callback → RtSink::write_* (downmix→mono f32, rtrb push, Relaxed counters)
                                   ▼ rtrb SPSC, 2–4 s
dsp thread (QoS utility; wakes every 20–80 ms; never spins)
  ├ rubato Fft 48k→16k → earshot VAD (16 ms) → 2 s pre-roll
  ├ wake-word inference only while VAD=speech (duty cycle)
  └ after wake: Opus 20 ms → bounded mpsc try_send ─▶ tokio WebSocket task (backpressure)
supervisor (core, 1 s tick): signals, heartbeat stall, drops, digital silence,
  default-input poll, sleep/wake → drop + rebuild capture through the port
```

## 2. Hard rules for code running in the audio callback

1. No allocation/deallocation (`Vec::push`, `format!`, `Box`, dropping the last `Arc`), no locks,
   no channels that may lock, no `log`/`tracing`/`println!`, no syscalls/sleep, no `Waker::wake`.
2. Only: format conversion, downmix, `rtrb::Producer::push_partial_slice`, `fetch_add(Relaxed)`.
   Resampling, VAD, inference, encoding run on the dsp thread (rubato README: "No heavy processing
   such as resampling should be performed here" [primary]).
3. Ring full → drop newest + count `frames_dropped`. Never block, never overwrite.
4. **The cpal error callback may run on the RT thread** (CoreAudio overload listener; cpal calls it via
   `Mutex::try_lock` and may drop errors) [primary: cpal 0.18.2 `macos/mod.rs`]. Only set atomic flags.
5. Preallocate scratch for the max callback size and loop for larger callbacks; `BufferSize::Fixed` is
   only a request [primary: cpal docs].
6. Prove it: counting allocator in tests (`assert_no_alloc` is a **no-op in release** by default
   [primary]); RTSan (`rtsan-standalone`, supports macOS) on the M2; loom for custom atomics — with
   `loom::thread::yield_now()` in every poll loop (without it loom ran 1 iteration and missed a planted
   `Relaxed` bug; with it, 42 iterations caught it [exp]).

## 3. The port (core; compiled against cpal =0.18.2 + rtrb 0.4.1 [exp])

```rust
pub struct AudioFormat { pub sample_rate_hz: u32, pub channels: u16 }
pub enum MicPermission { Granted, Denied, NotDetermined, Restricted, Unknown }
pub enum AudioError { PermissionDenied, NoInputDevice, DeviceBusy, Unsupported(String), Backend(String) }
pub mod signal { pub const DEVICE_LOST: u32 = 1; pub const STREAM_INVALIDATED: u32 = 2;
                 pub const DEVICE_CHANGED: u32 = 4; pub const XRUN: u32 = 8; pub const BACKEND_ERROR: u32 = 16; }
#[derive(Default)] pub struct CaptureStats { pub callbacks: AtomicU64, pub frames_in: AtomicU64,
                                              pub frames_dropped: AtomicU64, pub signals: AtomicU32 }
pub struct RtSink { /* rtrb::Producer<f32>, Box<[f32]> scratch, Arc<CaptureStats> */ }
impl RtSink {
    pub fn new(cap_frames: usize, max_cb_frames: usize) -> (Self, rtrb::Consumer<f32>, Arc<CaptureStats>);
    pub fn write_interleaved_f32(&mut self, d: &[f32], ch: usize); // RT-safe
    pub fn write_interleaved_i16(&mut self, d: &[i16], ch: usize); // RT-safe
}
pub trait AudioSource: Send {
    fn default_input_id(&self) -> Result<Option<String>, AudioError>; // cheap, polled
    fn mic_permission(&self) -> MicPermission;                         // never prompts
    fn open(&mut self, sink: RtSink) -> Result<Box<dyn ActiveCapture>, AudioError>; // drop = stop
}
pub trait ActiveCapture: Send { fn format(&self) -> AudioFormat; fn device_id(&self) -> &str; }
```
Full code, cpal glue (stream owned and dropped on one `audio-ctl` thread), supervisor:
`references/port-and-porting.md`.

| **Core** (pure Rust, CI-tested with fakes) | **OS adapter** |
|---|---|
| `RtSink`, `CaptureStats`, ring sizing, supervisor (rebuild/backoff, stall, silence) | Build/own/drop the stream; map sample formats |
| Resampler, VAD, wake word, pre-roll, Opus, framing, `VoiceUplink` port | Default-input query/listener, transport (Bluetooth?) query |
| Thread *intent* (`ThreadClass::Dsp`) | Apply QoS; permission query/request; sleep/wake events |

## 4. Decisions (versions as of 2026-11-12)

| Need | Choose | Notes |
|---|---|---|
| Capture | `cpal = "=0.18.2"` | Call `play()` (no auto-start since 0.18). Unreleased 0.19: `play`→`start`, `CallbackInfo`, `ErrorKind::Xrun` removed [primary] |
| Ring | `rtrb 0.4.1` | wait-free, exact capacity; no blocking wait. `ringbuf 0.6.0` OK; never `async-ringbuf` on the RT side (wakes a Waker) [primary] |
| Resample | `rubato 5.0.1` `Fft::<f32>::new(48000,16000,960,1,FixedSync::Input)` + `process_into_buffer` | 0 allocs, ~0.05 % core [exp]; 4 major versions in 2026 — re-check API |
| VAD | `earshot 1.2.2` (256 samples @16 kHz) | pure Rust, 0 allocs, ~0.06 % core [exp]; accuracy claims vendor-only |
| Wake word | openWakeWord-format model, `tract-onnx 0.23.10` (pure Rust) or `ort 2.0.0-rc.15` | oWW **pre-trained models are CC BY-NC-SA 4.0** [primary] → train own; ~3.4 % core ungated [exp] |
| Opus | `opus 0.4.0` (→ `opusic-sys`, libopus 1.6.1, needs cmake) | 16 kHz mono, 20 ms (320 samples), VOIP, 16–24 kb/s, FEC/DTX off over TCP; 0 allocs [exp] |
| Uplink | `tokio-tungstenite 0.30.0` | shrink `read_buffer_size` (128 KiB eager); `send` (feed+flush) [primary] |

Format: open the device at its **default** rate/format (macOS 0.18.2 input: F32/I16 only); never
request 16 kHz from hardware — cpal sets the device nominal rate, affecting other apps, and any rate
change surfaces as `StreamInvalidated` [primary]. Downmix in `RtSink`, resample on the dsp thread.

## 5. Device events (macOS, untested)

- cpal 0.18.2 **input does not follow the default input device** ("Input is not automatically
  rerouted"; only default output reroutes) [primary]. Poll `default_input_id()` (= stable
  `kAudioDevicePropertyDeviceUID`) every 2–5 s or add a `kAudioHardwarePropertyDefaultInputDevice`
  listener in the adapter; rebuild on change.
- cpal pauses the stream and signals `DeviceNotAvailable` (device gone) or `StreamInvalidated` (rate
  change) → drop + rebuild with backoff 250 ms→30 s, fresh ring, reset dsp state.
- Heartbeat: `callbacks` frozen > 2 s while running → rebuild (covers sleep/wake, silent HAL failures).
  Optional IOKit `IORegisterForSystemPower`: stop on WillSleep (must ack), reopen after
  `kIOMessageSystemHasPoweredOn` [primary: header]. `Instant` suspend behaviour is unspecified [primary].
- Bluetooth: a headset mic tends to force call-quality audio / lower output rate [secondary] → default
  to the built-in mic (`kAudioDevicePropertyTransportType`; cpal 0.18.2 can't tell on macOS).

## 6. Permission & UX (macOS, untested)

- Needs `NSMicrophoneUsageDescription` **and** (hardened runtime) `com.apple.security.device.audio-input`;
  otherwise "the system terminates your app" or access is silently denied [primary/secondary].
- Per-user LaunchAgent in an app-like `LSUIElement` bundle (via `SMAppService`); never a LaunchDaemon.
  TCC behaviour for launchd agents is "basically" undocumented (Apple DTS) [secondary]. Packaging is
  owned by the `fogar-macos-daemon` skill.
- Not-granted capture can yield **silent samples, not errors** (AVCaptureDevice header) [primary;
  unverified for AUHAL] → supervisor flags exact-zero input > 5 s and re-checks `mic_permission()`.
- An **orange menu-bar dot** shows whenever the mic is in use [primary] — always-on means always shown.

## 7. CPU and energy

- dsp thread: `libc::pthread_set_qos_class_self_np(qos_class_t::QOS_CLASS_UTILITY, 0)`; never real-time.
  Don't promote threads on macOS — the HAL IO thread already is RT.
- No spinning or `pthread_yield_np` (Apple [primary]): park the dsp thread with a timeout at frame
  cadence. ORT spins by default → `with_intra_op_spinning(false)`, `with_inter_op_spinning(false)`,
  `with_intra_threads(1)`; rc.15 is `Session::builder(&env)` [primary].
- Duty cycle: wake-word inference only while VAD says speech (+1 s hangover), catching up from pre-roll
  to fill its 16×80 ms context. VAD costs ~1/55 of the oWW pipeline [exp]. Measure false rejects.
- Request ~10–20 ms callbacks (`BufferSize::Fixed(rate/100)` clamped to the range); buffer size is
  device-wide [primary: cpal]. tract allocated ~350×/80 ms frame [exp] → watch RSS over 72 h.

## 8. Pitfalls

- oww-rs's mic code resamples, allocates and logs inside the cpal callback — copy its math, not its I/O.
- Shipping oWW `alexa`/`hey_jarvis` models (non-commercial licence).
- `ProcessType=Background` (sibling-skill default) may starve the dsp thread → compare with Standard.
- `feed()` without `flush()`: 60-byte Opus packets sit in tungstenite's 128 KiB write buffer.
- macOS voice-processing I/O (AEC) changes the duplex device's output format [secondary] — don't use it.
- WASAPI default-device behaviour differs between cpal changelog and code → rebuild on any signal.
- tract 0.23: `to_plain_array_view`, `into_runnable()` returns `Arc` [exp].

## 9. Windows / Linux

| | macOS | Windows (WASAPI) | Linux |
|---|---|---|---|
| cpal host | CoreAudio | WASAPI; `realtime` feature promotes its thread (MMCSS) | ALSA by default; `pipewire`/`pulseaudio` features opt-in (PipeWire>Pulse>ALSA) |
| Default-device change | poll/listener + rebuild | reroute or invalidate (conflict) → rebuild | Pulse/PipeWire default source; still poll |
| Permission | TCC + entitlement + usage string | Privacy ▸ Microphone ▸ "Let desktop apps access…" | none (Flatpak portals only) |
| Run as | per-user LaunchAgent | per-user logon process, not a Session-0 service [unverified] | `systemd --user` |
| Cost | — | ~1–2 days (permission probe, priority, packaging) | ~2–3 days (features, packaging, distro tests); prefer `BufferSize::Default` until cpal 0.19 fixes |

## 10. Measure on the real M2 before trusting

Idle CPU and wakeups/s (mic open, silence vs closed) · energy (`powermetrics`, Activity Monitor) ·
delivered callback size · `frames_dropped` under load, ProcessType Background vs Standard · oWW cost
tract vs ORT CPU vs CoreML · false accepts/h and false rejects with gating on/off · rebuild time on
default switch, AirPods, USB unplug, sleep/wake · zeros vs error when mic permission is denied/revoked ·
TCC prompt attribution · RSS after 72 h · compile of the port for aarch64-apple-darwin.

## References (load on demand)
- `references/cpal-coreaudio.md` — cpal 0.18.2 macOS internals, formats, buffers, rebuild loop, BT, sleep
- `references/ring-buffers-and-loom.md` — ring choice, waking the consumer, loom/RTSan/no-alloc testing
- `references/resampling-format.md` — negotiation policy, rubato 5 API and code
- `references/opus-encoding.md` — crates, frame sizes, bitrates, DTX/FEC, packetization, numbers
- `references/vad-wakeword-inference.md` — VADs, wake-word engines, licences, runtimes, duty cycle, QoS
- `references/macos-tcc-launchagent.md` — mic TCC for a headless agent, revoke handling, power events
- `references/websocket-streaming.md` — VoiceUplink port, backpressure, framing, reconnect
- `references/port-and-porting.md` — full compiled port + cpal adapter, supervisor, Windows/Linux
- `references/power-and-measurement.md` — M2 measurement and soak plan, benchmark table
