//! Speaker output on cpal 0.18.2 / CoreAudio (spec S2 §D).
//!
//! ```text
//! core: write(pcm i16 @ format) -> i16->f32, resample to the device rate, rtrb push (non-blocking)
//!                                         | rtrb SPSC, 3 s at the device rate
//! CoreAudio IO thread (RT)  cpal output callback -> OutputReader: pop, fan out to channels, silence
//! ```
//! The default output at its default format; cpal reroutes a stream built on the default output
//! when the default changes. `write` never waits: it accepts what fits in the 3 s ring (2 s of
//! gateway lead + margin) and reports the rest as not accepted. `stop` (barge-in) is a watermark:
//! `queued_samples()` is 0 at once and the callback discards the old audio on its next run. One
//! thread per playback owns the stream from build to drop and sleeps on a channel meanwhile
//! (no timer, no wakeups).

use super::{
    capture::{is_fatal, map_cpal},
    dsp::{COUNTERS, OutputReader, PlaybackShared, Resampler},
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use fogar_ports::{
    audio::{ActivePlayback, AudioError, AudioFormat},
    power::{PowerPolicy, WorkClass},
};
use std::{
    sync::{Arc, atomic::Ordering, mpsc},
    thread,
    time::Duration,
};

/// Bounded output buffer (spec S2: ~3 s = 2 s gateway lead + margin).
const RING_SECONDS: usize = 3;
/// Input samples converted per step inside `write` (stack scratch).
const BLOCK: usize = 1_024;
const OPEN_DEADLINE: Duration = Duration::from_secs(5);
/// Accepted input rates: anything a TTS could plausibly send; the rest is refused up front.
const RATES: std::ops::RangeInclusive<u32> = 8_000..=192_000;

pub(crate) struct MacPlayback {
    format: AudioFormat,
    device_hz: u32,
    prod: rtrb::Producer<f32>,
    resampler: Option<Resampler>,
    /// Resampled samples waiting to be pushed (preallocated for one BLOCK).
    scratch: Vec<f32>,
    /// Device-rate samples pushed into the ring, total.
    pushed: u64,
    shared: Arc<PlaybackShared>,
    stop: Option<mpsc::SyncSender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl std::fmt::Debug for MacPlayback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacPlayback")
            .field("format", &self.format)
            .field("device_hz", &self.device_hz)
            .field("queued", &self.queued_samples())
            .finish_non_exhaustive()
    }
}

impl MacPlayback {
    /// Device-rate samples still in the ring and not discarded.
    fn queued_device(&self) -> u64 {
        let consumed = self.shared.consumed.load(Ordering::Acquire);
        let discarded = self.shared.discard_until.load(Ordering::Acquire);
        self.pushed.saturating_sub(consumed.max(discarded))
    }

    /// How many input samples are sure to fit, resampler backlog included.
    fn room(&self) -> usize {
        let free = self.prod.slots().saturating_sub(2) as u64;
        let (inp, dev) = (
            u64::from(self.format.sample_rate_hz),
            u64::from(self.device_hz),
        );
        let held = self.resampler.as_ref().map_or(0, |r| r.pending_input()) as u64;
        let fit = (free * inp / dev).saturating_sub(held + 2);
        usize::try_from(fit).unwrap_or(usize::MAX)
    }

    fn push_scratch(&mut self) {
        let n = self.scratch.len().min(self.prod.slots());
        if n > 0
            && let Ok(chunk) = self.prod.write_chunk_uninit(n)
        {
            let written = chunk.fill_from_iter(self.scratch.iter().copied());
            self.pushed += written as u64;
        }
        self.scratch.clear();
    }
}

impl ActivePlayback for MacPlayback {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn write(&mut self, pcm: &[i16]) -> usize {
        if self.shared.failed.load(Ordering::Relaxed) {
            return 0;
        }
        let n = pcm.len().min(self.room());
        let mut block = [0f32; BLOCK];
        for part in pcm[..n].chunks(BLOCK) {
            let block = &mut block[..part.len()];
            for (d, &s) in block.iter_mut().zip(part) {
                *d = f32::from(s) / 32_768.0;
            }
            match self.resampler.as_mut() {
                Some(r) => r.process(block, &mut self.scratch),
                None => self.scratch.extend_from_slice(block),
            }
            self.push_scratch();
        }
        n
    }

    fn stop(&mut self) {
        self.shared
            .discard_until
            .store(self.pushed, Ordering::Release);
        if let Some(r) = self.resampler.as_mut() {
            r.reset();
        }
    }

    fn queued_samples(&self) -> usize {
        let dev = u128::from(self.queued_device());
        let in_rate = dev * u128::from(self.format.sample_rate_hz) / u128::from(self.device_hz);
        let held = self.resampler.as_ref().map_or(0, |r| r.pending_input());
        usize::try_from(in_rate)
            .unwrap_or(usize::MAX)
            .saturating_add(held)
    }
}

impl Drop for MacPlayback {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Ready {
    device_hz: u32,
    prod: rtrb::Producer<f32>,
}

fn build(shared: Arc<PlaybackShared>) -> Result<(cpal::Stream, Ready), AudioError> {
    let host = cpal::default_host();
    let dev = host.default_output_device().ok_or(AudioError::NoDevice)?;
    let sup = dev.default_output_config().map_err(|e| map_cpal(&e))?;
    let cfg = sup.config();
    if cfg.channels == 0 || !RATES.contains(&cfg.sample_rate) {
        return Err(AudioError::FormatUnsupported(AudioFormat {
            sample_rate_hz: cfg.sample_rate,
            channels: cfg.channels,
        }));
    }
    let ring = usize::try_from(cfg.sample_rate).unwrap_or(48_000) * RING_SECONDS;
    let (prod, cons) = rtrb::RingBuffer::<f32>::new(ring);
    let mut reader = OutputReader::new(cons, cfg.channels, shared.clone());
    let failed = shared;
    let on_error = move |e: cpal::Error| {
        COUNTERS.device_errors.fetch_add(1, Ordering::Relaxed);
        if is_fatal(e.kind()) {
            failed.failed.store(true, Ordering::Relaxed);
        }
    };
    let device_hz = cfg.sample_rate;
    let stream = match sup.sample_format() {
        cpal::SampleFormat::F32 => dev.build_output_stream(
            cfg,
            move |d: &mut [f32], _: &cpal::OutputCallbackInfo| reader.fill(d),
            on_error,
            None,
        ),
        cpal::SampleFormat::I16 => dev.build_output_stream(
            cfg,
            move |d: &mut [i16], _: &cpal::OutputCallbackInfo| reader.fill(d),
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
    stream.play().map_err(|e| map_cpal(&e))?;
    Ok((stream, Ready { device_hz, prod }))
}

pub(crate) fn open(format: AudioFormat) -> Result<Box<dyn ActivePlayback>, AudioError> {
    if format.channels != 1 || !RATES.contains(&format.sample_rate_hz) {
        return Err(AudioError::FormatUnsupported(format));
    }
    let shared = Arc::new(PlaybackShared::default());
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<Ready, AudioError>>(1);
    let (stop_tx, stop_rx) = mpsc::sync_channel::<()>(1);
    let for_thread = shared.clone();
    let thread = thread::Builder::new()
        .name("fogar-audio-out".into())
        .spawn(move || {
            let _ = crate::MacPowerPolicy.apply_current_thread(WorkClass::Control);
            match build(for_thread) {
                Ok((stream, ready)) => {
                    if ready_tx.send(Ok(ready)).is_ok() {
                        // Parked until the handle is dropped (sender gone): zero wakeups.
                        let _ = stop_rx.recv();
                    }
                    drop(stream); // on the thread that built it
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        })
        .map_err(|e| AudioError::Backend(Box::new(e)))?;
    let ready = match ready_rx.recv_timeout(OPEN_DEADLINE) {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            let _ = thread.join();
            return Err(e);
        }
        Err(_) => {
            // Not joined: the thread exits on its own when its send fails.
            return Err(AudioError::Backend("opening the speaker timed out".into()));
        }
    };
    let resampler = if ready.device_hz == format.sample_rate_hz {
        None
    } else {
        match Resampler::new(format.sample_rate_hz, ready.device_hz, BLOCK) {
            Some(r) => Some(r),
            None => {
                drop(stop_tx);
                let _ = thread.join();
                return Err(AudioError::FormatUnsupported(format));
            }
        }
    };
    let scratch_cap = resampler
        .as_ref()
        .map_or(BLOCK, |r| r.max_output(BLOCK) + BLOCK);
    Ok(Box::new(MacPlayback {
        format,
        device_hz: ready.device_hz,
        prod: ready.prod,
        resampler,
        scratch: Vec::with_capacity(scratch_cap),
        pushed: 0,
        shared,
        stop: Some(stop_tx),
        thread: Some(thread),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odd_formats_are_refused_before_touching_the_device() {
        assert!(matches!(
            open(AudioFormat {
                sample_rate_hz: 24_000,
                channels: 2
            }),
            Err(AudioError::FormatUnsupported(_))
        ));
        assert!(matches!(
            open(AudioFormat::mono(1_000)),
            Err(AudioError::FormatUnsupported(_))
        ));
    }
}
