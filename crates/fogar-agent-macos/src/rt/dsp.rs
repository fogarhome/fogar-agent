//! OS-free audio plumbing for the macOS adapter (no CoreAudio, no `unsafe`): the real-time
//! writer/reader that live inside the cpal callbacks, the process-wide counters and a small
//! streaming resampler. Compiled and unit-tested on every host.
//!
//! Real-time rules (fogar-realtime-audio §2) for [`InputWriter`] and [`OutputReader`]: no
//! allocation or deallocation, no locks, no logging, no syscalls, no `Waker::wake`. They only
//! convert samples, move them through a preallocated `rtrb` SPSC ring and bump atomics (`Relaxed`
//! counters; the barge-in watermark and the consumed total use `Acquire`/`Release`).

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

/// Process-wide audio counters (one microphone, one speaker per agent), never reset. Read through
/// `fogar_agent_macos::audio_counters()`.
#[derive(Debug)]
pub(crate) struct Counters {
    /// Callbacks seen on any input stream.
    pub(crate) input_callbacks: AtomicU64,
    /// Device frames lost because the capture ring was full (input overrun).
    pub(crate) input_overrun_frames: AtomicU64,
    /// 20 ms chunks dropped because the core's stream was full.
    pub(crate) chunks_dropped: AtomicU64,
    /// Gaps in playback: the ring ran dry and audio resumed shortly after (an xrun the listener
    /// hears), not the normal silence at the end of an utterance.
    pub(crate) output_underruns: AtomicU64,
    /// Resampled output samples the ring had no room for (should stay 0: `write` sizes itself).
    pub(crate) output_samples_dropped: AtomicU64,
    /// Errors reported by the audio device (cpal error callback, any kind).
    pub(crate) device_errors: AtomicU64,
    /// Captures the adapter ended on its own (device lost, stalled, permission revoked).
    pub(crate) captures_aborted: AtomicU64,
    /// `open` refused at once because an earlier, timed-out open is still inside CoreAudio.
    pub(crate) opens_refused_in_flight: AtomicU64,
    /// Audio threads left running after `drop` because they did not stop within the deadline.
    pub(crate) threads_detached: AtomicU64,
}

pub(crate) static COUNTERS: Counters = Counters {
    input_callbacks: AtomicU64::new(0),
    input_overrun_frames: AtomicU64::new(0),
    chunks_dropped: AtomicU64::new(0),
    output_underruns: AtomicU64::new(0),
    output_samples_dropped: AtomicU64::new(0),
    device_errors: AtomicU64::new(0),
    captures_aborted: AtomicU64::new(0),
    opens_refused_in_flight: AtomicU64::new(0),
    threads_detached: AtomicU64::new(0),
};

/// Sample formats the CoreAudio input/output hands to cpal on macOS (0.18.2: F32 or I16).
pub(crate) trait Sample: Copy {
    fn to_f32(self) -> f32;
    fn from_f32(v: f32) -> Self;
    const SILENCE: Self;
}

impl Sample for f32 {
    fn to_f32(self) -> f32 {
        self
    }
    fn from_f32(v: f32) -> Self {
        v
    }
    const SILENCE: Self = 0.0;
}

impl Sample for i16 {
    fn to_f32(self) -> f32 {
        f32::from(self) / 32_768.0
    }
    fn from_f32(v: f32) -> Self {
        // `as` saturates on f32 -> i16; the clamp keeps the scale symmetric.
        (v.clamp(-1.0, 1.0) * 32_767.0) as i16
    }
    const SILENCE: Self = 0;
}

/// Moved into the cpal input callback: downmix interleaved frames to mono and push them into the
/// SPSC ring. When the ring is full the newest frames are dropped and counted (never block,
/// never overwrite). `heartbeat` counts this capture's callbacks (stall detection per capture).
pub(crate) struct InputWriter {
    prod: rtrb::Producer<f32>,
    channels: usize,
    inv: f32,
    heartbeat: Arc<AtomicU64>,
}

impl InputWriter {
    pub(crate) fn new(prod: rtrb::Producer<f32>, channels: u16, heartbeat: Arc<AtomicU64>) -> Self {
        let channels = usize::from(channels.max(1));
        Self {
            prod,
            channels,
            inv: 1.0 / channels as f32,
            heartbeat,
        }
    }

    /// Real-time safe.
    pub(crate) fn write<S: Sample>(&mut self, data: &[S]) {
        self.heartbeat.fetch_add(1, Ordering::Relaxed);
        COUNTERS.input_callbacks.fetch_add(1, Ordering::Relaxed);
        let frames = data.len() / self.channels;
        let n = frames.min(self.prod.slots());
        if n > 0
            && let Ok(chunk) = self.prod.write_chunk_uninit(n)
        {
            let (ch, inv) = (self.channels, self.inv);
            chunk.fill_from_iter(
                data.chunks_exact(ch)
                    .map(|f| f.iter().map(|s| s.to_f32()).sum::<f32>() * inv),
            );
        }
        let lost = frames - n;
        if lost > 0 {
            COUNTERS
                .input_overrun_frames
                .fetch_add(lost as u64, Ordering::Relaxed);
        }
    }
}

/// State shared between [`OutputReader`] (callback) and the playback handle (core side).
#[derive(Debug, Default)]
pub(crate) struct PlaybackShared {
    /// Samples taken off the ring by the callback (played or discarded), total.
    pub(crate) consumed: AtomicU64,
    /// Barge-in watermark: the callback discards everything pushed before this total.
    pub(crate) discard_until: AtomicU64,
    /// Output callbacks run (stall detection on the core side).
    pub(crate) callbacks: AtomicU64,
    /// The device reported a fatal error (gone, invalidated): playback accepts nothing more.
    pub(crate) failed: AtomicBool,
}

/// Silence (in device frames) after which resumed audio is a new utterance, not a gap.
const GAP_FRAMES_MAX: u64 = 12_000; // 250 ms at 48 kHz

/// Moved into the cpal output callback: pop mono samples, copy them to every channel, fill the
/// rest with silence. Real-time safe.
pub(crate) struct OutputReader {
    cons: rtrb::Consumer<f32>,
    channels: usize,
    shared: Arc<PlaybackShared>,
    consumed: u64,
    /// Silent frames written since the ring ran dry in the middle of audio (`None`: not dry).
    starved: Option<u64>,
}

impl OutputReader {
    pub(crate) fn new(
        cons: rtrb::Consumer<f32>,
        channels: u16,
        shared: Arc<PlaybackShared>,
    ) -> Self {
        Self {
            cons,
            channels: usize::from(channels.max(1)),
            shared,
            consumed: 0,
            starved: None,
        }
    }

    pub(crate) fn fill<S: Sample>(&mut self, out: &mut [S]) {
        self.shared.callbacks.fetch_add(1, Ordering::Relaxed);
        // Barge-in: drop what was queued before `stop()`; a stop is never a gap.
        let target = self.shared.discard_until.load(Ordering::Acquire);
        if self.consumed < target {
            let skip = usize::try_from(target - self.consumed)
                .unwrap_or(usize::MAX)
                .min(self.cons.slots());
            if let Ok(chunk) = self.cons.read_chunk(skip) {
                chunk.commit_all();
                self.consumed += skip as u64;
            }
            self.starved = None;
        }
        let frames = out.len() / self.channels;
        let avail = frames.min(self.cons.slots());
        if avail > 0
            && let Some(silent) = self.starved.take()
            && silent <= GAP_FRAMES_MAX
        {
            COUNTERS.output_underruns.fetch_add(1, Ordering::Relaxed);
        }
        let mut frames_out = out.chunks_exact_mut(self.channels);
        if let Ok(chunk) = self.cons.read_chunk(avail) {
            let (a, b) = chunk.as_slices();
            for (&v, frame) in a.iter().chain(b.iter()).zip(&mut frames_out) {
                frame.fill(S::from_f32(v));
            }
            chunk.commit_all();
            self.consumed += avail as u64;
        }
        for frame in frames_out {
            frame.fill(S::SILENCE);
        }
        let whole = frames * self.channels;
        out[whole..].fill(S::SILENCE);
        let silent = (frames - avail) as u64;
        if silent > 0 {
            self.starved = match self.starved {
                Some(s) => Some(s.saturating_add(silent)),
                None if avail > 0 => Some(silent), // ran dry inside this callback
                None => None,                      // idle: nothing was playing
            };
        }
        self.shared.consumed.store(self.consumed, Ordering::Release);
    }
}

const fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// Largest polyphase table accepted (floats): 1 MiB. Exotic rate pairs beyond it are refused.
const MAX_TABLE: usize = 1 << 18;

/// Streaming band-limited resampler with a fixed rational ratio `out/in = up/down`: a
/// Blackman-windowed sinc evaluated on a precomputed polyphase table (no FFT, no extra crates).
/// The low-pass sits at 90 % of the lower Nyquist, so 48 kHz -> 16 kHz does not alias. Each
/// output is centred `taps/2 - 1` input samples after the newest input it needs, which is the
/// group delay: 95 samples (about 2 ms) for 48 -> 16 kHz, 31 samples (about 1.3 ms) for
/// 24 -> 48 kHz. Allocates only in `new`, as long as callers feed at most `max_block` samples per
/// call into an `out` with [`Resampler::max_output`] spare capacity.
pub(crate) struct Resampler {
    up: usize,
    down: usize,
    taps: usize,
    table: Box<[f32]>,
    buf: Vec<f32>,
    /// Next output position relative to `buf[0]`, in units of 1/`up` input samples.
    pos: usize,
    max_block: usize,
}

impl std::fmt::Debug for Resampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Resampler({}/{}, {} taps)",
            self.up, self.down, self.taps
        )
    }
}

impl Resampler {
    /// `None` when a rate is 0 or the table would be unreasonably large.
    pub(crate) fn new(in_hz: u32, out_hz: u32, max_block: usize) -> Option<Self> {
        if in_hz == 0 || out_hz == 0 || max_block == 0 {
            return None;
        }
        let g = gcd(u64::from(in_hz), u64::from(out_hz));
        let up = usize::try_from(u64::from(out_hz) / g).ok()?;
        let down = usize::try_from(u64::from(in_hz) / g).ok()?;
        let ratio = (down as f64 / up as f64).max(1.0);
        let half = (32.0 * ratio).ceil() as usize;
        let taps = 2 * half;
        let size = up.checked_mul(taps)?;
        if size > MAX_TABLE {
            return None;
        }
        let fc = 0.5 * (up as f64 / down as f64).min(1.0) * 0.9;
        let mut table = vec![0f32; size].into_boxed_slice();
        let d = (half - 1) as f64;
        for p in 0..up {
            let row = &mut table[p * taps..(p + 1) * taps];
            let mut sum = 0.0f64;
            for (k, c) in row.iter_mut().enumerate() {
                let u = d + p as f64 / up as f64 - k as f64;
                let x = 2.0 * fc * u;
                let sinc = if x.abs() < 1e-12 {
                    1.0
                } else {
                    (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                };
                let w = u / half as f64;
                let win = 0.42
                    + 0.5 * (std::f64::consts::PI * w).cos()
                    + 0.08 * (2.0 * std::f64::consts::PI * w).cos();
                let h = 2.0 * fc * sinc * win;
                sum += h;
                *c = h as f32;
            }
            // Unity gain at DC for every phase.
            if sum.abs() > 1e-12 {
                for c in row.iter_mut() {
                    *c = (f64::from(*c) / sum) as f32;
                }
            }
        }
        Some(Self {
            up,
            down,
            taps,
            table,
            buf: Vec::with_capacity(taps + max_block),
            pos: 0,
            max_block,
        })
    }

    /// Upper bound of outputs for `n` more inputs (what `out` must have spare).
    pub(crate) fn max_output(&self, n: usize) -> usize {
        (self.buf.len() + n) * self.up / self.down + 2
    }

    /// Inputs held back (not yet turned into output), for `queued_samples`.
    pub(crate) fn pending_input(&self) -> usize {
        self.buf.len().saturating_sub(self.pos / self.up)
    }

    /// Forget all history (barge-in, new take).
    pub(crate) fn reset(&mut self) {
        self.buf.clear();
        self.pos = 0;
    }

    /// Resample `input`, appending to `out`.
    pub(crate) fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        for piece in input.chunks(self.max_block) {
            self.buf.extend_from_slice(piece);
            loop {
                let base = self.pos / self.up;
                if base + self.taps > self.buf.len() {
                    break;
                }
                let phase = self.pos % self.up;
                let coeffs = &self.table[phase * self.taps..(phase + 1) * self.taps];
                let window = &self.buf[base..base + self.taps];
                let y: f32 = coeffs.iter().zip(window).map(|(c, x)| c * x).sum();
                out.push(y);
                self.pos += self.down;
            }
            let used = (self.pos / self.up).min(self.buf.len());
            self.buf.drain(..used);
            self.pos -= used * self.up;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rt::test_support::assert_no_alloc;

    #[test]
    fn input_writer_downmixes_counts_overrun_and_never_allocates() {
        let (prod, mut cons) = rtrb::RingBuffer::<f32>::new(4800);
        let beat = Arc::new(AtomicU64::new(0));
        let mut w = InputWriter::new(prod, 2, beat.clone());
        let block: Vec<f32> = [0.5f32, -0.25].repeat(1024);
        let before = COUNTERS.input_overrun_frames.load(Ordering::Relaxed);
        assert_no_alloc("InputWriter::write", || w.write(&block));
        for _ in 0..10 {
            w.write(&block);
        }
        assert!(beat.load(Ordering::Relaxed) >= 11, "per-capture heartbeat");
        assert!(COUNTERS.input_overrun_frames.load(Ordering::Relaxed) > before);
        assert_eq!(cons.slots(), 4800);
        let c = cons.read_chunk(4800).unwrap();
        let (a, b) = c.as_slices();
        assert!(a.iter().chain(b).all(|&x| (x - 0.125).abs() < 1e-6));
        c.commit_all();
        let (prod, mut cons) = rtrb::RingBuffer::<f32>::new(8);
        let mut w = InputWriter::new(prod, 1, beat);
        w.write(&[i16::MIN, 0, 16_384]);
        assert_eq!(cons.pop().unwrap(), -1.0);
        assert_eq!(cons.pop().unwrap(), 0.0);
        assert_eq!(cons.pop().unwrap(), 0.5);
    }

    #[test]
    fn output_reader_fans_out_discards_on_stop_and_never_allocates() {
        let (mut prod, cons) = rtrb::RingBuffer::<f32>::new(64);
        let shared = Arc::new(PlaybackShared::default());
        let mut r = OutputReader::new(cons, 2, shared.clone());
        for i in 0..10 {
            prod.push(i as f32 / 10.0).unwrap();
        }
        let mut out = [9.0f32; 8];
        r.fill(&mut out);
        assert_eq!(out, [0.0, 0.0, 0.1, 0.1, 0.2, 0.2, 0.3, 0.3]);
        assert_eq!(shared.consumed.load(Ordering::Acquire), 4);
        assert_no_alloc("OutputReader::fill", || r.fill(&mut out));
        assert!(shared.callbacks.load(Ordering::Relaxed) >= 1);
        // Barge-in: everything pushed so far is discarded, the rest is silence.
        let pushed = 10;
        shared.discard_until.store(pushed, Ordering::Release);
        let mut out = [9i16; 6];
        r.fill(&mut out);
        assert_eq!(out, [0; 6]);
        assert_eq!(shared.consumed.load(Ordering::Acquire), pushed);
        prod.push(1.0).unwrap();
        let mut out = [9i16; 4];
        r.fill(&mut out);
        assert_eq!(out, [32_767, 32_767, 0, 0]);
    }

    /// Only a short silence between two stretches of audio is an underrun; the end of an
    /// utterance, an idle device, a barge-in or a new utterance after a pause are not.
    #[test]
    fn underruns_count_only_real_gaps() {
        let (mut prod, cons) = rtrb::RingBuffer::<f32>::new(64_000);
        let shared = Arc::new(PlaybackShared::default());
        let mut r = OutputReader::new(cons, 1, shared.clone());
        let n = || COUNTERS.output_underruns.load(Ordering::Relaxed);
        let mut out = [0f32; 480];
        // Idle device, then audio starts: no gap.
        let base = n();
        r.fill(&mut out);
        for _ in 0..480 {
            prod.push(0.1).unwrap();
        }
        r.fill(&mut out);
        // Ring runs dry mid-callback, audio resumes 10 ms later: one gap.
        for _ in 0..100 {
            prod.push(0.1).unwrap();
        }
        r.fill(&mut out);
        r.fill(&mut out);
        for _ in 0..480 {
            prod.push(0.1).unwrap();
        }
        r.fill(&mut out);
        let after_gap = n();
        assert!(after_gap > base, "a short gap is counted");
        // Utterance ends, long pause (> 250 ms), next utterance: not a gap. Other tests may
        // bump the global counter concurrently, so only this reader's events are asserted
        // through its own state.
        for _ in 0..100 {
            prod.push(0.1).unwrap();
        }
        r.fill(&mut out);
        for _ in 0..40 {
            r.fill(&mut out);
        }
        assert!(r.starved.is_some_and(|s| s > GAP_FRAMES_MAX));
        for _ in 0..480 {
            prod.push(0.1).unwrap();
        }
        r.fill(&mut out);
        assert!(r.starved.is_none());
        // A barge-in clears the starving state.
        for _ in 0..100 {
            prod.push(0.1).unwrap();
        }
        r.fill(&mut out);
        assert!(r.starved.is_some());
        shared.discard_until.store(u64::MAX, Ordering::Release);
        r.fill(&mut out);
        assert!(r.starved.is_none());
    }

    fn tone(rate: u32, hz: f64, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                (0.5 * (2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(rate)).sin()) as f32
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / x.len() as f64).sqrt()
    }

    fn run(in_hz: u32, out_hz: u32, input: &[f32], block: usize) -> Vec<f32> {
        let mut r = Resampler::new(in_hz, out_hz, 1_024).unwrap();
        let mut out = Vec::with_capacity(r.max_output(input.len()) + 8);
        for piece in input.chunks(block) {
            r.process(piece, &mut out);
        }
        out
    }

    #[test]
    fn resampler_ratio_and_passband() {
        for &(i, o) in &[
            (48_000u32, 16_000u32),
            (44_100, 16_000),
            (24_000, 48_000),
            (24_000, 44_100),
            (16_000, 16_000),
        ] {
            let n = i as usize; // one second
            let out = run(i, o, &tone(i, 1_000.0, n), 441);
            let expect = (n as u64 * u64::from(o) / u64::from(i)) as usize;
            assert!(
                out.len().abs_diff(expect) <= 200,
                "{i}->{o}: {} outputs, expected ~{expect}",
                out.len()
            );
            // 1 kHz passes at unity (RMS of a 0.5 sine = 0.354), away from the edges.
            let mid = &out[out.len() / 4..out.len() * 3 / 4];
            assert!(
                (rms(mid) - 0.3536).abs() < 0.01,
                "{i}->{o}: passband rms {}",
                rms(mid)
            );
        }
    }

    /// 48 -> 16 kHz: speech band kept, everything at or above the new Nyquist (8 kHz) removed,
    /// swept finely around the edge where aliasing would fold back into the speech band.
    #[test]
    fn resampler_sweep_around_the_8_khz_edge() {
        let gain = |hz: f64| {
            let out = run(48_000, 16_000, &tone(48_000, hz, 48_000), 960);
            rms(&out[4_000..12_000]) / 0.3536
        };
        for hz in [300.0, 1_000.0, 3_400.0, 5_000.0, 6_000.0] {
            let g = gain(hz);
            assert!((g - 1.0).abs() < 0.02, "{hz} Hz passband gain {g}");
        }
        for hz in [
            8_000.0, 8_100.0, 8_200.0, 8_400.0, 9_000.0, 12_000.0, 20_000.0,
        ] {
            let g = gain(hz);
            assert!(g < 0.01, "{hz} Hz must be stopped (-40 dB), gain {g}");
        }
    }

    /// The output does not depend on how the input is cut into calls.
    #[test]
    fn resampler_output_is_block_invariant() {
        let input = tone(48_000, 1_234.0, 9_600);
        let whole = run(48_000, 16_000, &input, input.len());
        for block in [1, 7, 320, 441, 960, 1_024, 5_000] {
            assert_eq!(run(48_000, 16_000, &input, block), whole, "block {block}");
        }
        let input = tone(24_000, 700.0, 4_800);
        let whole = run(24_000, 44_100, &input, input.len());
        for block in [1, 13, 480, 2_000] {
            assert_eq!(run(24_000, 44_100, &input, block), whole, "block {block}");
        }
    }

    #[test]
    fn resampler_steady_state_is_alloc_free_and_reset_clears() {
        let mut r = Resampler::new(48_000, 16_000, 960).unwrap();
        let input = tone(48_000, 440.0, 960);
        let mut out = Vec::with_capacity(r.max_output(960) * 4);
        r.process(&input, &mut out);
        assert_no_alloc("Resampler::process", || {
            out.clear();
            r.process(&input, &mut out);
        });
        assert!(r.pending_input() > 0);
        r.reset();
        assert_eq!(r.pending_input(), 0);
        assert!(Resampler::new(0, 16_000, 1).is_none());
        assert!(Resampler::new(48_000, 16_000, 0).is_none());
        assert!(
            Resampler::new(1, 999_983, 1).is_none(),
            "huge table refused"
        );
    }
}
