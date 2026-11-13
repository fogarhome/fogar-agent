# The AudioSource port, the shared cpal adapter, and porting to Windows/Linux

The code below **compiles against `cpal = "=0.18.2"` and `rtrb = "0.4.1"`** (Linux/ALSA build in the
research container, rustc 1.97) and its RT-path test passes [exp]. It has **not** been compiled for
`aarch64-apple-darwin` (target unavailable offline) — compile on the Mac before relying on it,
especially `Send` bounds of `cpal::Host` on CoreAudio [unverified].

## Crate layout (fits the hexagonal workspace of `fogar-hexagonal-slices`)
```
crates/
  fogar-audio-core/     # no OS deps: RtSink, CaptureStats, AudioSource/ActiveCapture traits,
                        # supervisor state machine, dsp pipeline (rubato, earshot, wake word, opus),
                        # VoiceUplink trait. Deps: rtrb, rubato, earshot, opus, tract-onnx|ort
  fogar-audio-cpal/     # shared cpal glue (this file's `cpal_adapter`), cfg-free
  fogar-audio-macos/    # wraps cpal glue + AVFoundation permission, transport type, IOKit power,
                        # default-input listener, QoS application
  fogar-audio-windows/  # wraps cpal glue + Windows privacy check, priority/EcoQoS
  fogar-audio-linux/    # wraps cpal glue (features "pipewire","pulseaudio"), nice/systemd
  fogar-uplink-ws/      # tokio-tungstenite VoiceUplink adapter (portable)
```
The core is testable with a `FakeAudioSource` that pushes WAV fixtures through `RtSink` from a normal
thread (same code path the OS callback uses).

## Code (verbatim from the compiled experiment)
```rust
//! Compile-check of the AudioSource port + RT sink proposed in the skill.
//! `core` = platform-independent; `cpal_adapter` = shared cpal glue that the
//! macOS / Windows / Linux adapters wrap. Compiled on Linux against cpal 0.18.2.

pub mod core {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::sync::Arc;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct AudioFormat {
        pub sample_rate_hz: u32,
        pub channels: u16,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum MicPermission {
        Granted,
        Denied,
        NotDetermined,
        Restricted,
        Unknown,
    }

    #[derive(Debug)]
    pub enum AudioError {
        PermissionDenied,
        NoInputDevice,
        DeviceBusy,
        Unsupported(String),
        Backend(String),
    }

    /// Bit flags the adapter may raise from ANY thread, including the RT thread.
    pub mod signal {
        pub const DEVICE_LOST: u32 = 1 << 0;
        pub const STREAM_INVALIDATED: u32 = 1 << 1; // rate change, route change
        pub const DEVICE_CHANGED: u32 = 1 << 2;
        pub const XRUN: u32 = 1 << 3;
        pub const BACKEND_ERROR: u32 = 1 << 4;
    }

    /// Shared counters: written wait-free by the RT side, read by the supervisor.
    #[derive(Default, Debug)]
    pub struct CaptureStats {
        pub callbacks: AtomicU64,      // heartbeat: supervisor detects stalls
        pub frames_in: AtomicU64,
        pub frames_dropped: AtomicU64, // ring full -> consumer too slow
        pub signals: AtomicU32,        // OR of `signal::*`
    }

    impl CaptureStats {
        pub fn raise(&self, s: u32) {
            self.signals.fetch_or(s, Ordering::Relaxed);
        }
        pub fn take_signals(&self) -> u32 {
            self.signals.swap(0, Ordering::Relaxed)
        }
    }

    /// The ONLY object handed to the OS audio callback. All methods are
    /// wait-free, allocation-free, lock-free, syscall-free.
    pub struct RtSink {
        prod: rtrb::Producer<f32>,
        scratch: Box<[f32]>, // preallocated mono scratch, max callback size
        stats: Arc<CaptureStats>,
    }

    impl RtSink {
        pub fn new(capacity_frames: usize, max_cb_frames: usize) -> (Self, rtrb::Consumer<f32>, Arc<CaptureStats>) {
            let (prod, cons) = rtrb::RingBuffer::new(capacity_frames);
            let stats = Arc::new(CaptureStats::default());
            (
                RtSink { prod, scratch: vec![0.0; max_cb_frames].into_boxed_slice(), stats: stats.clone() },
                cons,
                stats,
            )
        }

        pub fn stats(&self) -> Arc<CaptureStats> {
            self.stats.clone()
        }

        /// Interleaved f32 -> mono (average) -> ring. Drops (and counts) on overflow.
        #[inline]
        pub fn write_interleaved_f32(&mut self, data: &[f32], channels: usize) {
            self.write_with(data.len() / channels.max(1), |i| {
                let fr = &data[i * channels..(i + 1) * channels];
                fr.iter().sum::<f32>() / channels as f32
            });
        }

        #[inline]
        pub fn write_interleaved_i16(&mut self, data: &[i16], channels: usize) {
            self.write_with(data.len() / channels.max(1), |i| {
                let fr = &data[i * channels..(i + 1) * channels];
                fr.iter().map(|&s| s as f32 / 32768.0).sum::<f32>() / channels as f32
            });
        }

        #[inline]
        fn write_with(&mut self, frames: usize, mut f: impl FnMut(usize) -> f32) {
            self.stats.callbacks.fetch_add(1, Ordering::Relaxed);
            let mut done = 0;
            while done < frames {
                // callback larger than scratch: process in pieces (no allocation)
                let n = (frames - done).min(self.scratch.len());
                for k in 0..n {
                    self.scratch[k] = f(done + k);
                }
                let (pushed, rest) = self.prod.push_partial_slice(&self.scratch[..n]);
                if !rest.is_empty() {
                    self.stats.frames_dropped.fetch_add(rest.len() as u64, Ordering::Relaxed);
                }
                self.stats.frames_in.fetch_add(pushed.len() as u64, Ordering::Relaxed);
                done += n;
            }
        }
    }

    /// Driven port. Implemented once per OS adapter crate.
    pub trait AudioSource: Send {
        /// Cheap; called by the supervisor every few seconds (default-device following).
        fn default_input_id(&self) -> Result<Option<String>, AudioError>;
        /// Non-prompting permission query.
        fn mic_permission(&self) -> MicPermission;
        /// Opens + starts capture on the CURRENT default input, moving `sink`
        /// into the OS callback. Dropping the returned handle stops capture.
        fn open(&mut self, sink: RtSink) -> Result<Box<dyn ActiveCapture>, AudioError>;
    }

    pub trait ActiveCapture: Send {
        fn format(&self) -> AudioFormat;
        fn device_id(&self) -> &str;
    }
}

pub mod cpal_adapter {
    //! Shared cpal glue (used by macOS/Windows/Linux adapters). The cpal Stream
    //! is created, owned and dropped on ONE dedicated control thread, because
    //! (a) CoreAudio listeners must be added/removed on the same thread and
    //! (b) Stream Send/Sync guarantees differ across cpal versions/backends.
    use super::core::*;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::sync::mpsc;
    use std::time::Duration;

    pub struct CpalSource {
        host: cpal::Host,
    }

    impl CpalSource {
        pub fn new() -> Self {
            CpalSource { host: cpal::default_host() }
        }
    }

    struct Handle {
        format: AudioFormat,
        id: String,
        stop_tx: mpsc::Sender<()>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl ActiveCapture for Handle {
        fn format(&self) -> AudioFormat {
            self.format
        }
        fn device_id(&self) -> &str {
            &self.id
        }
    }
    impl Drop for Handle {
        fn drop(&mut self) {
            let _ = self.stop_tx.send(());
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn map_err(e: cpal::Error) -> AudioError {
        match e.kind() {
            cpal::ErrorKind::PermissionDenied => AudioError::PermissionDenied,
            cpal::ErrorKind::DeviceBusy => AudioError::DeviceBusy,
            cpal::ErrorKind::DeviceNotAvailable => AudioError::NoInputDevice,
            cpal::ErrorKind::UnsupportedConfig => AudioError::Unsupported(e.to_string()),
            _ => AudioError::Backend(e.to_string()),
        }
    }

    impl AudioSource for CpalSource {
        fn default_input_id(&self) -> Result<Option<String>, AudioError> {
            match self.host.default_input_device() {
                None => Ok(None),
                Some(d) => d.id().map(|id| Some(id.to_string())).map_err(map_err),
            }
        }

        fn mic_permission(&self) -> MicPermission {
            MicPermission::Unknown // OS-specific adapters override (AVFoundation on macOS)
        }

        fn open(&mut self, mut sink: RtSink) -> Result<Box<dyn ActiveCapture>, AudioError> {
            let (ready_tx, ready_rx) = mpsc::channel::<Result<(AudioFormat, String), AudioError>>();
            let (stop_tx, stop_rx) = mpsc::channel::<()>();
            let thread = std::thread::Builder::new()
                .name("audio-ctl".into())
                .spawn(move || {
                    let host = cpal::default_host();
                    let built = (|| {
                        let dev = host.default_input_device().ok_or(AudioError::NoInputDevice)?;
                        let id = dev.id().map_err(map_err)?.to_string();
                        let sup = dev.default_input_config().map_err(map_err)?;
                        let mut cfg = sup.config();
                        // Ask for ~10-20 ms callbacks; fewer wake-ups than tiny buffers.
                        if let cpal::SupportedBufferSize::Range { min, max } = sup.buffer_size() {
                            let want = (cfg.sample_rate / 100).clamp(*min, *max); // 10 ms
                            cfg.buffer_size = cpal::BufferSize::Fixed(want);
                        }
                        let ch = cfg.channels as usize;
                        let fmt = AudioFormat { sample_rate_hz: cfg.sample_rate, channels: cfg.channels };
                        // error callback may run on the RT thread (CoreAudio overload
                        // listener): only atomic flag writes here.
                        let stats_for_err = sink.stats();
                        let err_cb = move |e: cpal::Error| {
                            use super::core::signal::*;
                            let s = match e.kind() {
                                cpal::ErrorKind::DeviceNotAvailable => DEVICE_LOST,
                                cpal::ErrorKind::StreamInvalidated => STREAM_INVALIDATED,
                                cpal::ErrorKind::DeviceChanged => DEVICE_CHANGED,
                                cpal::ErrorKind::Xrun => XRUN,
                                _ => BACKEND_ERROR,
                            };
                            stats_for_err.raise(s);
                        };
                        let timeout = Some(Duration::from_secs(2));
                        let stream = match sup.sample_format() {
                            cpal::SampleFormat::F32 => dev.build_input_stream(
                                cfg,
                                move |d: &[f32], _: &cpal::InputCallbackInfo| sink.write_interleaved_f32(d, ch),
                                err_cb,
                                timeout,
                            ),
                            cpal::SampleFormat::I16 => dev.build_input_stream(
                                cfg,
                                move |d: &[i16], _: &cpal::InputCallbackInfo| sink.write_interleaved_i16(d, ch),
                                err_cb,
                                timeout,
                            ),
                            other => return Err(AudioError::Unsupported(format!("{other:?}"))),
                        }
                        .map_err(map_err)?;
                        stream.play().map_err(map_err)?; // 0.18: streams do not auto-start
                        Ok((stream, fmt, id))
                    })();
                    match built {
                        Ok((stream, fmt, id)) => {
                            let _ = ready_tx.send(Ok((fmt, id)));
                            let _ = stop_rx.recv(); // park until stop
                            drop(stream); // dropped on the same thread that built it
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                        }
                    }
                })
                .map_err(|e| AudioError::Backend(e.to_string()))?;
            let (format, id) = ready_rx.recv().map_err(|e| AudioError::Backend(e.to_string()))??;
            Ok(Box::new(Handle { format, id, stop_tx, thread: Some(thread) }))
        }
    }

}
```

### RT-path test (passes) [exp]
```rust
use port_check::core::RtSink;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
struct C;
static N: AtomicU64 = AtomicU64::new(0);
unsafe impl GlobalAlloc for C {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 { N.fetch_add(1, Ordering::Relaxed); unsafe { System.alloc(l) } }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) { unsafe { System.dealloc(p, l) } }
}
#[global_allocator] static G: C = C;

#[test]
fn rt_sink_is_alloc_free_and_counts_drops() {
    let (mut sink, mut cons, stats) = RtSink::new(4800, 512);
    let block = vec![0.25f32; 1024 * 2]; // stereo, larger than scratch on purpose
    let before = N.load(Ordering::Relaxed);
    for _ in 0..10 { sink.write_interleaved_f32(&block, 2); } // 10240 frames into 4800 ring
    let after = N.load(Ordering::Relaxed);
    assert_eq!(after - before, 0, "RT path allocated");
    assert_eq!(stats.frames_in.load(Ordering::Relaxed), 4800);
    assert_eq!(stats.frames_dropped.load(Ordering::Relaxed), 10240 - 4800);
    let mut out = vec![0f32; 4800];
    let (got, _) = cons.pop_partial_slice(&mut out);
    assert!(got.iter().all(|&x| (x - 0.25).abs() < 1e-6));
}
```

## Design notes
- `RtSink` is the *only* thing moved into the OS callback; it is core code, so every adapter gets the
  same proven RT path (and the same counters) for free. Adapters only map sample formats.
- The stream is owned by a dedicated `audio-ctl` thread and dropped there (CoreAudio listeners must be
  added/removed on the same thread [primary: cpal source comment]; cpal ≤0.18 `Stream` Send/Sync differs
  per backend).
- The error callback only ORs bits into `CaptureStats.signals` (it may run on the RT thread).
- `open()` uses the **current default** input; a device-selection policy (e.g. "prefer built-in, avoid
  Bluetooth") belongs in the OS adapter because transport detection is OS-specific; expose it as an
  `InputSelection { FollowDefault, PreferBuiltIn, ById(String) }` parameter on the adapter constructor.
- Format handling: macOS 0.18.2 default input configs are F32/I16 only; Windows/Linux defaults may be
  I32/I24/F64 → extend the `match` (or choose an F32 config from `supported_input_configs()`).
- `mic_permission()` default `Unknown`; macOS adapter implements it with AVFoundation; Windows adapter
  can only infer (capture failure / silence) [unverified]; Linux returns `Granted` unless sandboxed.

## Supervisor (core) sketch
```rust
loop {
    thread::park_timeout(Duration::from_secs(1));
    let sig = stats.take_signals();
    let cb = stats.callbacks.load(Relaxed);
    let stalled = cb == last_cb && running_for > Duration::from_secs(2);
    let default_moved = policy.follows_default() && tick % 3 == 0
        && source.default_input_id()?.as_deref() != Some(handle.device_id());
    if sig & (signal::DEVICE_LOST | signal::STREAM_INVALIDATED | signal::DEVICE_CHANGED) != 0
        || stalled || default_moved {
        drop(handle);                       // stop + join control thread
        let (sink, cons, st) = RtSink::new(rate_guess * 3, 4096);
        handle = backoff.retry(|| source.open(sink))?;  // 250 ms, x2, cap 30 s
        dsp_tx.send(DspCmd::NewInput { cons, format: handle.format() })?;
        stats = st;
    }
    last_cb = cb;
}
```
(`RtSink` is moved into `open`, so a retry must build a fresh sink each attempt.)

## Windows adapter (WASAPI via cpal) — what it needs
- Same cpal glue. Enable cpal feature `realtime` only if you want cpal to promote its capture thread
  (it calls `audio_thread_priority::promote_current_thread_to_real_time`, falling back to
  `THREAD_PRIORITY_TIME_CRITICAL`) [primary: cpal wasapi/stream.rs]. WASAPI's own capture thread is
  cpal's thread, so promotion matters more here than on macOS.
- Default-device changes: cpal 0.18.0 changelog says default streams "automatically reroute"; the
  0.18.2 code says "WASAPI never rebinds the IAudioClient" and reports `StreamInvalidated` or
  `DeviceNotAvailable`; master reverts an earlier change about `DeviceChanged` [primary: conflict].
  The supervisor's rebuild-on-any-signal path covers all variants — test on Windows.
- Permission: Settings ▸ Privacy & security ▸ Microphone, including "Let desktop apps access your
  microphone", per-app settings exist [primary: Microsoft Support]. How a denied desktop app's WASAPI
  capture fails (error vs silence) is [unverified] → same silence detector.
- Run as a per-user process (logon task / startup entry), not a Session-0 service, so it sees the user's
  audio endpoints and privacy settings [unverified].
- Min Windows 10 (cpal master) [primary: changelog].
- Cost: ~1–2 days (permission probe, priority/EcoQoS, packaging), audio code unchanged.

## Linux adapter (ALSA / PulseAudio / PipeWire via cpal)
- cpal default features are **empty**: a plain build uses ALSA only (works through the Pulse/PipeWire
  ALSA plugin `default`). Enable `features = ["pipewire"]` (and/or `"pulseaudio"`) for native hosts;
  with them the default host priority is PipeWire > PulseAudio > ALSA [primary: Cargo.toml + 0.18.0
  changelog]. Build deps: `libasound2-dev` (ALSA), PipeWire/Pulse client dev packages.
- 0.18.x fixes worth knowing: PipeWire capture read from wrong offset on some devices (fixed on
  master); PulseAudio `BufferSize::Fixed` input stopped after the first callback (fixed on master);
  ALSA "Try to resume from hardware after a system suspend" [primary: changelog]. → Prefer
  `BufferSize::Default` on Linux until 0.19.
- RT promotion via rtkit needs `realtime-dbus` (D-Bus dev libs) — not needed for capture-only.
- Default-device following: Pulse/PipeWire default source routing; still poll `default_input_id()`.
- No mic permission model outside Flatpak portals; run as `systemd --user` service.
- Cost: ~2–3 days, mostly packaging and testing across PipeWire/Pulse/ALSA-only systems.

## Porting-cost summary
| Item | macOS | Windows | Linux |
|---|---|---|---|
| RtSink / dsp / wake / opus / uplink | 0 (core) | 0 | 0 |
| cpal glue | shared | shared (+format arms) | shared (+features) |
| Permission probe | AVFoundation (objc2) | inference only | n/a |
| Device policy (built-in vs BT) | CoreAudio transport type | MMDevice form factor [unverified API] | PipeWire node props [unverified] |
| Power events | IOKit | `WM_POWERBROADCAST`/`RegisterSuspendResumeNotification` [unverified names] | logind `PrepareForSleep` D-Bus signal [unverified] |
| Thread QoS | pthread QoS | priority classes / EcoQoS | nice |
