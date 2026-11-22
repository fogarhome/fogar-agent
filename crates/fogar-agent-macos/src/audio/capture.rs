//! Microphone capture on cpal 0.18.2 / CoreAudio (spec S2 §D).
//!
//! ```text
//! CoreAudio IO thread (RT)  cpal input callback -> InputWriter: downmix, rtrb push, counters
//!                                         | rtrb SPSC, 2 s at the device rate
//! fogar-audio-in (QoS utility, 20 ms)     resample -> 16 kHz, cut 20 ms chunks, EventSink::push
//! ```
//! One thread per capture owns the cpal stream from build to drop (CoreAudio listeners must be
//! added and removed on the same thread) and also does the DSP, so a capture costs one thread
//! and ~50 wakeups/s only while it is open. The device is the default input AT `open` time, at
//! its default format (never 16 kHz requested from hardware). A default-device change during a
//! capture is out of S2 (S3); a lost/invalidated device or stalled callbacks END the capture
//! (the chunk stream yields `None`), and the core decides what to do.

use super::dsp::{COUNTERS, InputWriter, Resampler};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use fogar_ports::{
    audio::{ActiveCapture, AudioChunk, AudioError, AudioFormat},
    events::{EventSink, EventStream, Pushed, event_channel},
    power::{PowerPolicy, WorkClass},
};
use std::{
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

/// Rate of the chunks the port delivers (spec S2: 16 kHz mono f32).
pub(crate) const CAPTURE_HZ: u32 = 16_000;
/// 20 ms at 16 kHz.
const CHUNK: usize = 320;
/// DSP cadence: one wakeup per chunk.
const TICK: Duration = Duration::from_millis(20);
/// Ring between the RT callback and the DSP thread.
const RING_SECONDS: usize = 2;
/// Most samples popped per resampler call.
const BLOCK: usize = 1_024;
/// No input callback for this long while open = stalled (sleep, HAL failure): end the capture.
const STALL: Duration = Duration::from_secs(2);
/// Deadline for building and starting the stream.
const OPEN_DEADLINE: Duration = Duration::from_secs(5);

/// Error-callback flags (the error callback may run on the RT thread: atomics only).
const SIG_FATAL: u32 = 1;

pub(crate) fn map_cpal(e: &cpal::Error) -> AudioError {
    match e.kind() {
        cpal::ErrorKind::PermissionDenied => AudioError::PermissionDenied,
        cpal::ErrorKind::DeviceBusy => AudioError::DeviceBusy,
        cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::HostUnavailable => {
            AudioError::NoDevice
        }
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
    stop: Option<mpsc::SyncSender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ActiveCapture for MacCapture {
    fn format(&self) -> AudioFormat {
        AudioFormat::mono(CAPTURE_HZ)
    }
}

impl Drop for MacCapture {
    fn drop(&mut self) {
        // Dropping the sender wakes the thread at once (Disconnected); join so the device is
        // closed (and the orange dot gone) when `drop` returns.
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Built {
    stream: cpal::Stream,
    cons: rtrb::Consumer<f32>,
    rate: u32,
    signals: Arc<AtomicU32>,
}

fn build() -> Result<Built, AudioError> {
    let host = cpal::default_host();
    let dev = host.default_input_device().ok_or(AudioError::NoDevice)?;
    let sup = dev.default_input_config().map_err(|e| map_cpal(&e))?;
    let mut cfg = sup.config();
    if cfg.channels == 0 || cfg.sample_rate == 0 {
        return Err(AudioError::FormatUnsupported(AudioFormat {
            sample_rate_hz: cfg.sample_rate,
            channels: cfg.channels,
        }));
    }
    // ~10 ms callbacks: fewer wakeups than tiny buffers, low enough latency. Only a request.
    if let cpal::SupportedBufferSize::Range { min, max } = *sup.buffer_size()
        && min <= max
    {
        cfg.buffer_size = cpal::BufferSize::Fixed((cfg.sample_rate / 100).clamp(min, max));
    }
    let rate = cfg.sample_rate;
    let ring = usize::try_from(rate).unwrap_or(48_000) * RING_SECONDS;
    let (prod, cons) = rtrb::RingBuffer::<f32>::new(ring);
    let mut writer = InputWriter::new(prod, cfg.channels);
    let signals = Arc::new(AtomicU32::new(0));
    let sig = signals.clone();
    let on_error = move |e: cpal::Error| {
        COUNTERS.device_errors.fetch_add(1, Ordering::Relaxed);
        if is_fatal(e.kind()) {
            sig.fetch_or(SIG_FATAL, Ordering::Relaxed);
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
        _ => {
            return Err(AudioError::FormatUnsupported(AudioFormat {
                sample_rate_hz: cfg.sample_rate,
                channels: cfg.channels,
            }));
        }
    }
    .map_err(|e| map_cpal(&e))?;
    stream.play().map_err(|e| map_cpal(&e))?; // 0.18: streams do not auto-start
    Ok(Built {
        stream,
        cons,
        rate,
        signals,
    })
}

/// Opens the default input. The caller has already checked the permission (port contract).
pub(crate) fn open(
    capacity: NonZeroUsize,
) -> Result<(Box<dyn ActiveCapture>, EventStream<AudioChunk>), AudioError> {
    let (sink, stream) = event_channel(capacity);
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), AudioError>>(1);
    let (stop_tx, stop_rx) = mpsc::sync_channel::<()>(1);
    let thread = thread::Builder::new()
        .name("fogar-audio-in".into())
        .spawn(move || {
            let _ = crate::MacPowerPolicy.apply_current_thread(WorkClass::Control);
            let built = match build() {
                Ok(b) => b,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            if ready_tx.send(Ok(())).is_err() {
                return; // `open` gave up (deadline): drop the stream here, on its own thread.
            }
            run(built, sink, &stop_rx);
        })
        .map_err(|e| AudioError::Backend(Box::new(e)))?;
    match ready_rx.recv_timeout(OPEN_DEADLINE) {
        Ok(Ok(())) => Ok((
            Box::new(MacCapture {
                stop: Some(stop_tx),
                thread: Some(thread),
            }),
            stream,
        )),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            // The thread notices on its next send and exits by itself; not joined (it may be
            // stuck inside CoreAudio, which is exactly why we stopped waiting).
            Err(AudioError::Backend(
                "opening the microphone timed out".into(),
            ))
        }
    }
}

fn run(b: Built, sink: EventSink<AudioChunk>, stop: &mpsc::Receiver<()>) {
    let Built {
        stream,
        mut cons,
        rate,
        signals,
    } = b;
    let mut resampler = if rate == CAPTURE_HZ {
        None
    } else {
        Resampler::new(rate, CAPTURE_HZ, BLOCK)
    };
    if rate != CAPTURE_HZ && resampler.is_none() {
        COUNTERS.captures_aborted.fetch_add(1, Ordering::Relaxed);
        drop(stream);
        return;
    }
    // Everything the ring can hold, at 16 kHz, plus one chunk: allocated once.
    let ring = cons.buffer().capacity();
    let mut pending: Vec<f32> =
        Vec::with_capacity(ring * CAPTURE_HZ as usize / rate as usize + CHUNK + 64);
    let mut last_callbacks = COUNTERS.input_callbacks.load(Ordering::Relaxed);
    let mut last_progress = Instant::now();
    let aborted = 'outer: loop {
        match stop.recv_timeout(TICK) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break false,
        }
        if signals.load(Ordering::Relaxed) & SIG_FATAL != 0 {
            break true;
        }
        let cb = COUNTERS.input_callbacks.load(Ordering::Relaxed);
        if cb != last_callbacks {
            last_callbacks = cb;
            last_progress = Instant::now();
        } else if last_progress.elapsed() > STALL {
            break true;
        }
        while cons.slots() > 0 {
            let n = cons.slots().min(BLOCK);
            let Ok(chunk) = cons.read_chunk(n) else { break };
            let (a, c) = chunk.as_slices();
            for part in [a, c] {
                match resampler.as_mut() {
                    Some(r) => r.process(part, &mut pending),
                    None => pending.extend_from_slice(part),
                }
            }
            chunk.commit_all();
        }
        let mut sent = 0;
        while pending.len() - sent >= CHUNK {
            let samples = pending[sent..sent + CHUNK].to_vec();
            sent += CHUNK;
            match sink.push(AudioChunk { samples }) {
                Pushed::Delivered => {}
                Pushed::DroppedFull => {
                    COUNTERS.chunks_dropped.fetch_add(1, Ordering::Relaxed);
                }
                // The core dropped its stream: nobody listens, stop the device.
                Pushed::Closed => break 'outer false,
            }
        }
        pending.drain(..sent);
    };
    if aborted {
        COUNTERS.captures_aborted.fetch_add(1, Ordering::Relaxed);
    }
    // Stream first (device closed, callback and its ring producer freed on this thread), then
    // the sink: the core sees `None` only once the microphone is really off.
    drop(stream);
    drop(sink);
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
        let m = |k| map_cpal(&cpal::Error::from(k));
        assert!(matches!(
            m(cpal::ErrorKind::PermissionDenied),
            AudioError::PermissionDenied
        ));
        assert!(matches!(
            m(cpal::ErrorKind::DeviceBusy),
            AudioError::DeviceBusy
        ));
        assert!(matches!(
            m(cpal::ErrorKind::DeviceNotAvailable),
            AudioError::NoDevice
        ));
        assert!(matches!(m(cpal::ErrorKind::Other), AudioError::Backend(_)));
    }
}
