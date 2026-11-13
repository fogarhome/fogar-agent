# cpal on macOS / CoreAudio — what the code actually does (cpal 0.18.2)

Source read: `RustAudio/cpal` tag `v0.18.2` (released 2026-08-16) and `master` at 40ff3ad
(2026-11-11, unreleased 0.19.0). Nothing here was run on a Mac: **untested on macOS**.

## Versions and API facts [primary: source + CHANGELOG]

- Latest release `0.18.2`. `master` is `0.19.0` (unreleased, edition 2024, MSRV 1.85) with breaking
  changes: `StreamTrait::play` → `start` (play deprecated), `InputCallbackInfo`/`OutputCallbackInfo`
  merged into `CallbackInfo`, `CallbackInfo::xrun()` added, `ErrorKind::Xrun` **removed**,
  `DeviceTrait`/`StreamTrait: Send + Sync`, duplex streams. → Pin `cpal = "=0.18.2"` and keep all cpal
  calls inside the adapter crate so the 0.19 migration touches one file.
- 0.18.0 changes that matter: unified `cpal::Error` with `.kind()` → `ErrorKind::{DeviceBusy,
  DeviceChanged, DeviceNotAvailable, HostUnavailable, InvalidInput, PermissionDenied, RealtimeDenied,
  ResourceExhausted, StreamInvalidated, UnsupportedConfig, UnsupportedOperation, Xrun, BackendError}`;
  `build_*_stream(config: StreamConfig /*by value*/, data_cb, err_cb, timeout: Option<Duration>)`;
  **streams no longer start automatically — call `play()`**; default configs prefer 48 kHz then
  44.1 kHz; `StreamConfig` is `Copy`; `sample_rate` is a plain `u32` (compiled [exp]);
  `Device: PartialEq+Eq+Hash+Display`, `device.id() -> Result<DeviceId, Error>`.
- Input callback signature (0.18.2): `FnMut(&[T], &cpal::InputCallbackInfo) + Send + 'static`,
  `T: SizedSample`. Error callback: `FnMut(cpal::Error) + Send + 'static`.
- Cargo default features are empty. `realtime` = Android/Windows thread promotion;
  `realtime-dbus` = Linux rtkit. Neither is needed on macOS (the HAL IO thread is already
  real-time [secondary: CoreAudio architecture docs; not re-verified]).

## How a macOS input stream is built (device.rs `build_input_stream_raw`)

1. `set_physical_format(device, rate, channels, fmt)`; if that fails, `set_sample_rate(...)` —
   **cpal changes the hardware nominal rate if your config differs** → affects every app using the
   mic. Always use the device's current/default rate.
2. AUHAL `AudioUnit` bound to that `AudioDeviceID` (`AudioUnitMode::Input`). Comment in source:
   "Input is not automatically rerouted, so its buffer depth is constant". → **no default-input
   following**. (Default *output* uses the DefaultOutput unit and does follow.)
3. Stream format: interleaved; only **F32 or I16** accepted for macOS default configs in 0.18.2
   ("Sample format is not supported; supported formats are F32 and I16"). master fixes
   `supported_configs()` reporting only F32.
4. `BufferSize::Fixed(n)`: validated against `kAudioDevicePropertyBufferFrameSizeRange`, then set
   via `kAudioDevicePropertyBufferFrameSize` with **Global scope — a device-level property**
   (source comment). `BufferSize::Default` "may be surprisingly large"; Fixed is a *request*:
   "The callback size may vary between calls" (lib.rs docs).
5. Callback: reads `mBuffers[0]`, builds `Data`, converts host time; on time conversion error it
   calls `try_emit_error` **from the RT thread**.
6. A `DisconnectManager` spawns a thread that registers three property listeners on the device
   (listeners must be added/removed on the same thread — source comment):
   - `kAudioDevicePropertyDeviceIsAlive` → `ErrorKind::DeviceNotAvailable`
   - `kAudioDevicePropertyNominalSampleRate` → `ErrorKind::StreamInvalidated` (any rate change)
   - `kAudioDeviceProcessorOverload` → `ErrorKind::Xrun` (comment: "Overload notifications fire on
     the RT thread")
   A second thread ("cpal-coreaudio-disconnect") receives these, `try_lock`s the stream, **pauses it**
   and calls your error callback.
7. The error callback is stored as `Arc<Mutex<dyn FnMut(Error)+Send>>`; `try_emit_error` uses
   `try_lock` and drops the error if contended. → Your error callback can run on the RT thread
   and can miss events: make it set atomic flags only, and back it with the heartbeat watchdog.

## Callback constraints (all OS) — why

The OS audio thread has a hard deadline of one buffer period (e.g. 480 frames @48 kHz = 10 ms).
Any operation with unbounded latency (heap allocation, lock, page fault on first touch, logging,
file/socket I/O, waking a futex/Waker) risks an overload/xrun and, worse, priority inversion with
lower-priority threads holding the lock [secondary: R. Bencina, "Real-time audio programming 101:
time waits for nothing", 2011; rubato README; cpal docs]. Practical list for fogar:

| Allowed in callback | Forbidden |
|---|---|
| arithmetic, format conversion, downmix | `Vec::new/push/collect`, `Box`, `format!`, `to_string` |
| `rtrb::Producer::push_partial_slice` / `write_chunk_uninit` | `Mutex::lock`, `RwLock`, `std::sync::mpsc::Sender::send` |
| `AtomicU64::fetch_add(Relaxed)`, `AtomicU32::fetch_or` | `log!`, `tracing` events, `println!` |
| reading preallocated, pre-touched buffers | `Waker::wake`, tokio channels, `thread::spawn`, `sleep` |
| | dropping the last `Arc`, dropping `Vec`s (dealloc) |

## Sample formats and conversion

- Prefer F32 if the device default is F32 (typical on macOS) [unverified for the M2 built-in mic].
- I16 → f32: `s as f32 / 32768.0`. If you ever accept I32/I24 on other OSes (0.18 default config may
  return them on high-precision hardware [primary: changelog]): `I32` → `/2147483648.0`; cpal's `I24`
  type uses 4 bytes with 24 significant bits.
- Downmix by averaging channels in the callback (cheap, halves ring traffic for stereo).

## Buffer size guidance (energy vs latency)

- Wake word does not need low latency; 10–20 ms callbacks are fine. Each callback is a wake-up of
  the RT thread; fewer, larger callbacks = fewer wake-ups (energy) [inference — measure].
- The buffer frame size is device-wide; other apps (e.g. a DAW, a call) may request smaller sizes.
  The HAL behaviour when clients disagree is [unverified]; do not fight it: request
  `rate/100 .. rate/50` frames clamped to the reported `SupportedBufferSize::Range`, and size
  scratch buffers for the **max** of that range (cpal may still deliver other sizes).
- Ring capacity: 2–4 s of mono f32 at the device rate (48k × 4 B × 4 s ≈ 768 KB). It absorbs dsp
  thread stalls (QoS throttling, page-ins of model weights).

## Rebuild loop (supervisor, core)

```
state Running(handle, dev_id):
  every 1–5 s:
    sig = stats.take_signals()
    if sig & (DEVICE_LOST|STREAM_INVALIDATED|DEVICE_CHANGED) → rebuild
    if callbacks unchanged for > 2 s                         → rebuild (stall: sleep/wake, HAL hang)
    if source.default_input_id()? != dev_id (and policy=follow-default) → rebuild
    if frames_dropped grew                                    → metric + log (NOT from RT thread)
    if exact-zero input for > 5 s                             → PossiblyNoPermission → check mic_permission()
rebuild: drop(handle)  // joins control thread; stream dropped on its creating thread
         create new RtSink + ring (allocation is fine here) ; open(); backoff 250 ms ×2 … 30 s on error
```
Dropping and recreating the ring on rebuild avoids stale-sample splicing. The dsp thread must
accept a "new consumer + new format" message and reset resampler/VAD/wake-word state.

## Sleep / wake (untested)

- Expected: the HAL stops IO during sleep; on wake the IOProc resumes or the device disappears
  (USB/Bluetooth) [unverified]. The heartbeat rule covers both.
- Explicit hooks: `objc2-io-kit 0.3.2` exposes `IORegisterForSystemPower` (feature `libc`),
  `IOAllowPowerChange`, `kIOMessageSystemWillSleep` (must be acknowledged, else sleep is delayed up to
  30 s), `kIOMessageSystemHasPoweredOn` ("1-5 or more seconds after initiating system wakeup")
  [primary: header text in objc2-io-kit]. Needs a CFRunLoop thread for the notification port.
  Recommended: stop capture on WillSleep, rebuild 1–2 s after HasPoweredOn. Owner: the macOS adapter
  of a `SystemEvents` port (see `fogar-macos-daemon`).

## Bluetooth / route changes (untested)

- Firefox maintainers observed AirPods output dropping 48 kHz → 24 kHz during mic use (with the
  voice-processing unit) [secondary: Mozilla bug 1964935]; users widely report "call quality"
  headset audio while a mic is open [secondary]. For an always-on listener this would degrade the
  user's music all day. Policy: capture from the built-in mic unless the user explicitly selects
  another device; detect transport with `kAudioDevicePropertyTransportType` →
  `kAudioDeviceTransportTypeBluetooth` / `...BuiltIn` (constants present in objc2-core-audio 0.3.2
  [primary]). cpal 0.18.2 `DeviceDescription::interface_type()` on CoreAudio only marks aggregate
  devices; master reports the transport [primary: changelog].
- Selecting a non-default device: `host.device_by_id(&DeviceId)` (0.18+) or iterate
  `input_devices()` and compare `id()`.
- Never open the mic in "hog mode" or change the device rate.

## master/0.19 migration checklist
`play()`→`start()`; `&cpal::InputCallbackInfo`→`&cpal::CallbackInfo`; replace `ErrorKind::Xrun`
handling with `info.xrun()` inside the callback (set an atomic); `Stream: Send + Sync` lets you drop
the control-thread indirection only if CoreAudio listener-thread rules allow it (keep it).
