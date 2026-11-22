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
//! `queued_samples()` is 0 at once and the callback discards the old audio on its next run. The
//! device thread (`rt::worker`) owns the stream and sleeps on a channel (no timer, no wakeups).
//!
//! A dead output (fatal device error, or no callback for 2 s after a 6 s start grace) accepts
//! nothing and reports nothing queued, so the core does not wait on audio that will never play.
//! `is_alive()` reports it (port health, issue #25). Death is latched: a handle that stalled
//! stays dead even if the callbacks come back (so nothing queued before the stall plays late);
//! the core drops it and reopens (follow-up in fogar-core, S2 3c).

use super::capture::{is_fatal, map_cpal};
use crate::rt::{
    dsp::{COUNTERS, OutputReader, PlaybackShared, Resampler},
    pump::{FIRST_CALLBACK_GRACE, STALL},
    worker::{self, Ctx},
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use fogar_ports::{
    audio::{ActivePlayback, AudioError, AudioFormat},
    power::{PowerPolicy, WorkClass},
};
use std::{
    cell::Cell,
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

/// Bounded output buffer (spec S2: ~3 s = 2 s gateway lead + margin).
const RING_SECONDS: usize = 3;
/// Input samples converted per step inside `write` (stack scratch).
const BLOCK: usize = 1_024;
/// Accepted rates: anything a TTS could plausibly send; the rest is refused up front.
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
    opened: Instant,
    /// Last callback count seen and when it changed (`None`: no callback yet).
    beat: Cell<(u64, Option<Instant>)>,
    /// Latched once `dead()` is true: a dead handle stays dead (port contract, issue #25).
    died: Cell<bool>,
    _worker: worker::Worker,
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
    /// The device will not play what is queued: fatal error, or callbacks stopped. Latched:
    /// once dead, this handle stays dead and the core reopens (port `is_alive`, issue #25).
    fn dead(&self) -> bool {
        if self.died.get() {
            return true;
        }
        let dead = self.probe_dead();
        if dead {
            self.died.set(true);
        }
        dead
    }

    fn probe_dead(&self) -> bool {
        if self.shared.failed.load(Ordering::Relaxed) {
            return true;
        }
        let now = Instant::now();
        let cb = self.shared.callbacks.load(Ordering::Relaxed);
        let (seen, at) = self.beat.get();
        if cb != seen {
            self.beat.set((cb, Some(now)));
            return false;
        }
        match at {
            None => now.duration_since(self.opened) > FIRST_CALLBACK_GRACE,
            Some(t) => now.duration_since(t) > STALL,
        }
    }

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
        // `room()` sizes every write so this stays 0; counted if the estimate is ever wrong.
        let lost = self.scratch.len() - n;
        if lost > 0 {
            COUNTERS
                .output_samples_dropped
                .fetch_add(lost as u64, Ordering::Relaxed);
        }
        self.scratch.clear();
    }
}

impl ActivePlayback for MacPlayback {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn write(&mut self, pcm: &[i16]) -> usize {
        if self.dead() {
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

    fn is_alive(&self) -> bool {
        !self.dead()
    }

    fn queued_samples(&self) -> usize {
        if self.dead() {
            return 0;
        }
        let dev = u128::from(self.queued_device());
        let in_rate = dev * u128::from(self.format.sample_rate_hz) / u128::from(self.device_hz);
        let held = self.resampler.as_ref().map_or(0, |r| r.pending_input());
        usize::try_from(in_rate)
            .unwrap_or(usize::MAX)
            .saturating_add(held)
    }
}

struct Ready {
    device_hz: u32,
    prod: rtrb::Producer<f32>,
}

fn body(ctx: Ctx<Ready>, shared: Arc<PlaybackShared>) {
    let _ = crate::MacPowerPolicy.apply_current_thread(WorkClass::Control);
    let host = cpal::default_host();
    let Some(dev) = host.default_output_device() else {
        return ctx.fail(AudioError::NoDevice);
    };
    let sup = match dev.default_output_config() {
        Ok(s) => s,
        Err(e) => return ctx.fail(map_cpal(&e, None)),
    };
    let cfg = sup.config();
    let format = AudioFormat {
        sample_rate_hz: cfg.sample_rate,
        channels: cfg.channels,
    };
    if cfg.channels == 0 || !RATES.contains(&cfg.sample_rate) {
        return ctx.fail(AudioError::FormatUnsupported(format));
    }
    let ring = usize::try_from(cfg.sample_rate).unwrap_or(48_000) * RING_SECONDS;
    let (prod, cons) = rtrb::RingBuffer::<f32>::new(ring);
    let mut reader = OutputReader::new(cons, cfg.channels, shared.clone());
    // May run on the RT thread: atomics only.
    let on_error = move |e: cpal::Error| {
        COUNTERS.device_errors.fetch_add(1, Ordering::Relaxed);
        if is_fatal(e.kind()) {
            shared.failed.store(true, Ordering::Relaxed);
        }
    };
    let built = match sup.sample_format() {
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
        _ => return ctx.fail(AudioError::FormatUnsupported(format)),
    };
    let stream = match built {
        Ok(s) => s,
        Err(e) => return ctx.fail(map_cpal(&e, Some(format))),
    };
    if !ctx.built() || !ctx.starting() {
        return; // abandoned: the stream drops here, unstarted
    }
    if let Err(e) = stream.play() {
        return ctx.start_failed(map_cpal(&e, None));
    }
    if ctx.ready(Ready {
        device_hz: format.sample_rate_hz,
        prod,
    }) {
        // Parked until the handle is dropped (sender gone): zero wakeups.
        let _ = ctx.stop.recv();
    }
    drop(stream); // on the thread that built it
}

pub(crate) fn open(format: AudioFormat) -> Result<Box<dyn ActivePlayback>, AudioError> {
    Ok(Box::new(open_device(format)?))
}

fn open_device(format: AudioFormat) -> Result<MacPlayback, AudioError> {
    if format.channels != 1 || !RATES.contains(&format.sample_rate_hz) {
        return Err(AudioError::FormatUnsupported(format));
    }
    let shared = Arc::new(PlaybackShared::default());
    let for_thread = shared.clone();
    let (w, ready) = worker::spawn(
        "fogar-audio-out",
        &worker::OUTPUT,
        worker::OPEN_DEADLINE,
        move |ctx| body(ctx, for_thread),
    )?;
    let resampler = if ready.device_hz == format.sample_rate_hz {
        None
    } else {
        // Both rates are within RATES, so the table is bounded; `None` only on a bug.
        Some(
            Resampler::new(format.sample_rate_hz, ready.device_hz, BLOCK)
                .ok_or(AudioError::FormatUnsupported(format))?,
        )
    };
    let scratch_cap = resampler
        .as_ref()
        .map_or(BLOCK, |r| r.max_output(BLOCK) + BLOCK);
    Ok(MacPlayback {
        format,
        device_hz: ready.device_hz,
        prod: ready.prod,
        resampler,
        scratch: Vec::with_capacity(scratch_cap),
        pushed: 0,
        shared,
        opened: Instant::now(),
        beat: Cell::new((0, None)),
        died: Cell::new(false),
        _worker: w,
    })
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

    /// With a device (the macOS CI runner has an output): a failed output accepts
    /// nothing and reports nothing queued, so the core never waits on audio that cannot play.
    #[test]
    fn a_dead_output_accepts_and_queues_nothing() {
        // Every Mac, including the CI runner, has an output: no device is a failure here.
        let mut pb = open_device(AudioFormat::mono(24_000)).expect("no output device");
        assert!(pb.write(&[0; 2_400]) > 0);
        assert!(pb.queued_samples() > 0 || pb.shared.callbacks.load(Ordering::Relaxed) > 0);
        assert!(pb.is_alive());
        pb.shared.failed.store(true, Ordering::Relaxed);
        fogar_testkit::contract::audio_sink::dead_playback_is_inert(&mut pb);
        pb.shared.failed.store(false, Ordering::Relaxed);
        assert!(!pb.is_alive(), "death is latched");
    }
}
