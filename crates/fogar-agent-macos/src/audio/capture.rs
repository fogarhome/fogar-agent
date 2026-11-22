//! Microphone capture on cpal 0.18.2 / CoreAudio (spec S2 §D).
//!
//! ```text
//! CoreAudio IO thread (RT)  cpal input callback -> InputWriter: downmix, rtrb push, heartbeat
//!                                         | rtrb SPSC, 2 s at the device rate
//! fogar-audio-in (QoS utility, 20 ms)     rt::pump: resample -> 16 kHz, 20 ms chunks, endings
//! ```
//! One thread per capture (`rt::worker`) owns the cpal stream from build to drop (CoreAudio
//! listeners must be added and removed on the same thread) and runs the pump, so a capture costs
//! one thread and ~50 wakeups/s only while it is open. The device is the default input AT `open`
//! time, at its default format (never 16 kHz requested from hardware). A default-device change
//! during a capture is out of S2 (S3). The capture ENDS (the chunk stream yields `None`, the core
//! decides) on a fatal device error, no first callback within 6 s, callbacks stalled for 2 s, or
//! the TCC permission leaving `Granted` (re-read every second).

use super::permission;
use crate::rt::{
    dsp::{COUNTERS, InputWriter},
    pump::{self, CAPTURE_HZ, Env, Pump, SIG_FATAL},
    worker::{self, Ctx},
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use fogar_ports::{
    audio::{ActiveCapture, AudioChunk, AudioError, AudioFormat, MicPermission},
    events::{EventSink, EventStream, event_channel},
    power::{PowerPolicy, WorkClass},
};
use std::{
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// DSP cadence: one wakeup per 20 ms chunk.
const TICK: Duration = Duration::from_millis(20);
/// Ring between the RT callback and the DSP thread.
const RING_SECONDS: usize = 2;

/// cpal error -> port category. `format` is the configuration that was being opened.
pub(crate) fn map_cpal(e: &cpal::Error, format: Option<AudioFormat>) -> AudioError {
    match e.kind() {
        cpal::ErrorKind::PermissionDenied => AudioError::PermissionDenied,
        // DeviceBusy is the port's transient, retry-later category (no `Transient` variant yet).
        cpal::ErrorKind::DeviceBusy | cpal::ErrorKind::ResourceExhausted => AudioError::DeviceBusy,
        cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::HostUnavailable => {
            AudioError::NoDevice
        }
        cpal::ErrorKind::UnsupportedConfig => match format {
            Some(f) => AudioError::FormatUnsupported(f),
            None => AudioError::Backend(Box::new(e.clone())),
        },
        _ => AudioError::Backend(Box::new(e.clone())),
    }
}

/// Fatal for a running stream: it will deliver nothing more.
pub(crate) fn is_fatal(kind: cpal::ErrorKind) -> bool {
    matches!(
        kind,
        cpal::ErrorKind::DeviceNotAvailable
            | cpal::ErrorKind::StreamInvalidated
            | cpal::ErrorKind::HostUnavailable
            | cpal::ErrorKind::PermissionDenied
            | cpal::ErrorKind::BackendError
            | cpal::ErrorKind::Other
    )
}

#[derive(Debug)]
pub(crate) struct MacCapture {
    _worker: worker::Worker,
}

impl ActiveCapture for MacCapture {
    fn format(&self) -> AudioFormat {
        AudioFormat::mono(CAPTURE_HZ)
    }
}

/// Built and not started.
struct Built {
    stream: cpal::Stream,
    pump: Pump,
}

fn build() -> Result<Built, AudioError> {
    let host = cpal::default_host();
    let dev = host.default_input_device().ok_or(AudioError::NoDevice)?;
    let sup = dev.default_input_config().map_err(|e| map_cpal(&e, None))?;
    let mut cfg = sup.config();
    let format = AudioFormat {
        sample_rate_hz: cfg.sample_rate,
        channels: cfg.channels,
    };
    if cfg.channels == 0 || cfg.sample_rate == 0 {
        return Err(AudioError::FormatUnsupported(format));
    }
    // ~10 ms callbacks: fewer wakeups than tiny buffers, low enough latency. Only a request.
    if let cpal::SupportedBufferSize::Range { min, max } = *sup.buffer_size()
        && min <= max
    {
        cfg.buffer_size = cpal::BufferSize::Fixed((cfg.sample_rate / 100).clamp(min, max));
    }
    let ring = usize::try_from(cfg.sample_rate).unwrap_or(48_000) * RING_SECONDS;
    let (prod, cons) = rtrb::RingBuffer::<f32>::new(ring);
    let heartbeat = Arc::new(AtomicU64::new(0));
    let signals = Arc::new(AtomicU32::new(0));
    let pump = Pump::new(cons, cfg.sample_rate, heartbeat.clone(), signals.clone())
        .ok_or(AudioError::FormatUnsupported(format))?;
    let mut writer = InputWriter::new(prod, cfg.channels, heartbeat);
    // May run on the RT thread (CoreAudio overload listener): atomics only.
    let on_error = move |e: cpal::Error| {
        COUNTERS.device_errors.fetch_add(1, Ordering::Relaxed);
        if is_fatal(e.kind()) {
            signals.fetch_or(SIG_FATAL, Ordering::Relaxed);
        }
    };
    let stream = match sup.sample_format() {
        cpal::SampleFormat::F32 => dev.build_input_stream(
            cfg,
            move |d: &[f32], _: &cpal::InputCallbackInfo| writer.write(d),
            on_error,
            None,
        ),
        cpal::SampleFormat::I16 => dev.build_input_stream(
            cfg,
            move |d: &[i16], _: &cpal::InputCallbackInfo| writer.write(d),
            on_error,
            None,
        ),
        _ => return Err(AudioError::FormatUnsupported(format)),
    }
    .map_err(|e| map_cpal(&e, Some(format)))?;
    Ok(Built { stream, pump })
}

/// Production world of the pump: the worker's stop channel, the monotonic clock, TCC.
struct RealEnv<'a> {
    stop: &'a mpsc::Receiver<()>,
    start: Instant,
}

impl Env for RealEnv<'_> {
    fn wait_tick(&mut self) -> bool {
        matches!(
            self.stop.recv_timeout(TICK),
            Err(mpsc::RecvTimeoutError::Timeout)
        )
    }
    fn now(&mut self) -> Duration {
        self.start.elapsed()
    }
    fn permission_ok(&mut self) -> bool {
        permission::status() == MicPermission::Granted
    }
}

fn body(ctx: Ctx<()>, sink: EventSink<AudioChunk>) {
    let _ = crate::MacPowerPolicy.apply_current_thread(WorkClass::Control);
    let b = match build() {
        Ok(b) => b,
        Err(e) => return ctx.fail(e),
    };
    // Abandoned by `open` (deadline): the stream drops here and the microphone never starts.
    if !ctx.built() || !ctx.starting() {
        return;
    }
    // 0.18: streams do not auto-start.
    if let Err(e) = b.stream.play() {
        return ctx.start_failed(map_cpal(&e, None));
    }
    if !ctx.ready(()) {
        return; // nobody waits any more: the stream drops here
    }
    let mut env = RealEnv {
        stop: &ctx.stop,
        start: Instant::now(),
    };
    let _ = pump::run(b.pump, b.stream, sink, &mut env);
}

/// Opens the default input. The caller has already checked the permission (port contract).
pub(crate) fn open(
    capacity: NonZeroUsize,
) -> Result<(Box<dyn ActiveCapture>, EventStream<AudioChunk>), AudioError> {
    let (sink, stream) = event_channel(capacity);
    let (w, ()) = worker::spawn(
        "fogar-audio-in",
        &worker::INPUT,
        worker::OPEN_DEADLINE,
        move |ctx| body(ctx, sink),
    )?;
    Ok((Box::new(MacCapture { _worker: w }), stream))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_loss_is_fatal_xruns_and_reroutes_are_not() {
        assert!(is_fatal(cpal::ErrorKind::DeviceNotAvailable));
        assert!(is_fatal(cpal::ErrorKind::StreamInvalidated));
        assert!(!is_fatal(cpal::ErrorKind::Xrun));
        assert!(!is_fatal(cpal::ErrorKind::DeviceChanged));
        assert!(!is_fatal(cpal::ErrorKind::RealtimeDenied));
    }

    #[test]
    fn cpal_errors_map_to_port_categories() {
        let f = AudioFormat::mono(48_000);
        let m = |k| map_cpal(&cpal::Error::from(k), Some(f));
        assert!(matches!(
            m(cpal::ErrorKind::PermissionDenied),
            AudioError::PermissionDenied
        ));
        assert!(matches!(
            m(cpal::ErrorKind::DeviceBusy),
            AudioError::DeviceBusy
        ));
        assert!(matches!(
            m(cpal::ErrorKind::ResourceExhausted),
            AudioError::DeviceBusy
        ));
        assert!(matches!(
            m(cpal::ErrorKind::DeviceNotAvailable),
            AudioError::NoDevice
        ));
        assert!(matches!(
            m(cpal::ErrorKind::UnsupportedConfig),
            AudioError::FormatUnsupported(x) if x == f
        ));
        assert!(matches!(m(cpal::ErrorKind::Other), AudioError::Backend(_)));
    }
}
