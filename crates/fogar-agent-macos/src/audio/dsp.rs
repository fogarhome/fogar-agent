//! Portable, OS-free audio plumbing for the macOS adapter (no CoreAudio, no `unsafe`): the
//! real-time writer/reader that live inside the cpal callbacks, and a small streaming resampler.
//!
//! Real-time rules (fogar-realtime-audio §2) for [`InputWriter`] and [`OutputReader`]: no
//! allocation or deallocation, no locks, no logging, no syscalls, no `Waker::wake`. They only
//! convert samples, move them through a preallocated `rtrb` SPSC ring and bump `Relaxed` counters
//! (the barge-in watermark uses `Acquire`/`Release`). Tests below prove the no-alloc part with a
//! counting allocator.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

/// Process-wide audio counters (one microphone, one speaker per agent). Read with
/// [`super::audio_counters`]; never reset.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    /// Callbacks seen on the input (also the capture heartbeat).
    pub(crate) input_callbacks: AtomicU64,
    /// Device frames lost because the capture ring was full (input overrun).
    pub(crate) input_overrun_frames: AtomicU64,
    /// 20 ms chunks dropped because the core's stream was full.
    pub(crate) chunks_dropped: AtomicU64,
    /// Output callbacks that ran dry in the middle of queued audio (audible gap, xrun).
    pub(crate) output_underruns: AtomicU64,
    /// Device errors reported by cpal (any kind, including xruns it reports itself).
    pub(crate) device_errors: AtomicU64,
    /// Captures ended by the adapter itself (device lost, stream invalidated, stalled callbacks).
    pub(crate) captures_aborted: AtomicU64,
}

pub(crate) static COUNTERS: Counters = Counters {
    input_callbacks: AtomicU64::new(0),
    input_overrun_frames: AtomicU64::new(0),
    chunks_dropped: AtomicU64::new(0),
    output_underruns: AtomicU64::new(0),
    device_errors: AtomicU64::new(0),
    captures_aborted: AtomicU64::new(0),
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
        // `as` saturates on f32 -> i16; the clamp keeps +1.0 from wrapping to the far side.
        (v.clamp(-1.0, 1.0) * 32_767.0) as i16
    }
    const SILENCE: Self = 0;
}

/// Moved into the cpal input callback: downmix interleaved frames to mono and push them into the
/// SPSC ring. When the ring is full the newest frames are dropped and counted (never block,
/// never overwrite).
pub(crate) struct InputWriter {
    prod: rtrb::Producer<f32>,
    channels: usize,
    inv: f32,
}

impl InputWriter {
    /// `channels` must be >= 1 (checked by the caller before the stream is built).
    pub(crate) fn new(prod: rtrb::Producer<f32>, channels: u16) -> Self {
        let channels = usize::from(channels.max(1));
        Self {
            prod,
            channels,
            inv: 1.0 / channels as f32,
        }
    }

    /// Real-time safe.
    pub(crate) fn write<S: Sample>(&mut self, data: &[S]) {
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
    /// The device reported a fatal error (gone, invalidated): playback accepts nothing more.
    pub(crate) failed: AtomicBool,
}

/// Moved into the cpal output callback: pop mono samples, copy them to every channel, fill the
/// rest with silence. Real-time safe.
pub(crate) struct OutputReader {
    cons: rtrb::Consumer<f32>,
    channels: usize,
    shared: Arc<PlaybackShared>,
    consumed: u64,
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
        }
    }

    pub(crate) fn fill<S: Sample>(&mut self, out: &mut [S]) {
        // Barge-in: drop what was queued before `stop()`.
        let target = self.shared.discard_until.load(Ordering::Acquire);
        if self.consumed < target {
            let skip = usize::try_from(target - self.consumed)
                .unwrap_or(usize::MAX)
                .min(self.cons.slots());
            if let Ok(chunk) = self.cons.read_chunk(skip) {
                chunk.commit_all();
                self.consumed += skip as u64;
            }
        }
        let frames = out.len() / self.channels;
        let avail = frames.min(self.cons.slots());
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
        // Trailing samples of a partial frame (should not happen) are silenced too.
        let whole = frames * self.channels;
        out[whole..].fill(S::SILENCE);
        if avail > 0 && avail < frames {
            COUNTERS.output_underruns.fetch_add(1, Ordering::Relaxed);
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
/// The low-pass sits at 90 % of the lower Nyquist, so 48 kHz -> 16 kHz does not alias. Its group
/// delay is `taps/2` input samples (2 ms at 48 kHz -> 16 kHz). Allocates only in `new`, as long
/// as callers feed at most `max_block` samples per call into an `out` with
/// [`Resampler::max_output`] spare capacity.
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
    use std::alloc::{GlobalAlloc, Layout, System};

    /// Counts allocations and frees made by the CURRENT thread only (tests run in parallel).
    struct Counting;
    thread_local! {
        // const-initialised, no destructor: touching it never allocates (safe inside `alloc`).
        static ALLOCS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    fn bump() {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
    }
    // SAFETY: forwards every call unchanged to the system allocator; only adds a counter.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            bump();
            // SAFETY: same layout contract as the caller's.
            unsafe { System.alloc(l) }
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            bump();
            // SAFETY: `p` came from `System.alloc` with this layout.
            unsafe { System.dealloc(p, l) }
        }
    }
    #[global_allocator]
    static A: Counting = Counting;

    fn allocs_during(mut f: impl FnMut()) -> u64 {
        let before = ALLOCS.with(|c| c.get());
        f();
        ALLOCS.with(|c| c.get()) - before
    }

    #[test]
    fn input_writer_downmixes_counts_overrun_and_never_allocates() {
        let (prod, mut cons) = rtrb::RingBuffer::<f32>::new(4800);
        let mut w = InputWriter::new(prod, 2);
        let block: Vec<f32> = [0.5f32, -0.25].repeat(1024);
        let before = COUNTERS.input_overrun_frames.load(Ordering::Relaxed);
        let n = allocs_during(|| w.write(&block));
        assert_eq!(n, 0, "the RT input path allocated");
        for _ in 0..10 {
            w.write(&block);
        }
        assert!(COUNTERS.input_overrun_frames.load(Ordering::Relaxed) > before);
        assert_eq!(cons.slots(), 4800);
        let c = cons.read_chunk(4800).unwrap();
        let (a, b) = c.as_slices();
        assert!(a.iter().chain(b).all(|&x| (x - 0.125).abs() < 1e-6));
        c.commit_all();
        let (prod, mut cons) = rtrb::RingBuffer::<f32>::new(8);
        let mut w = InputWriter::new(prod, 1);
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
        let n = allocs_during(|| {
            out = [9.0; 8];
            r.fill(&mut out);
        });
        assert_eq!(n, 0, "the RT output path allocated");
        let played = shared.consumed.load(Ordering::Acquire);
        assert!(played >= 4);
        // Barge-in: everything pushed so far is discarded, rest is silence.
        shared.discard_until.store(10, Ordering::Release);
        let mut out = [9i16; 6];
        r.fill(&mut out);
        assert_eq!(out, [0; 6]);
        assert_eq!(shared.consumed.load(Ordering::Acquire), 10);
        prod.push(1.0).unwrap();
        let mut out = [9i16; 4];
        r.fill(&mut out);
        assert_eq!(out, [32_767, 32_767, 0, 0]);
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
        let mut r = Resampler::new(in_hz, out_hz, block).unwrap();
        let mut out = Vec::with_capacity(r.max_output(input.len()) + 8);
        for piece in input.chunks(block) {
            r.process(piece, &mut out);
        }
        out
    }

    #[test]
    fn resampler_ratio_passband_and_stopband() {
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
        // 12 kHz at 48 kHz is above the 8 kHz Nyquist of 16 kHz: it must not alias back.
        let out = run(48_000, 16_000, &tone(48_000, 12_000.0, 48_000), 960);
        let mid = &out[4_000..12_000];
        assert!(rms(mid) < 0.005, "aliasing: rms {}", rms(mid));
    }

    #[test]
    fn resampler_steady_state_is_alloc_free_and_reset_clears() {
        let mut r = Resampler::new(48_000, 16_000, 960).unwrap();
        let input = tone(48_000, 440.0, 960);
        let mut out = Vec::with_capacity(r.max_output(960) * 4);
        r.process(&input, &mut out);
        let n = allocs_during(|| {
            out.clear();
            r.process(&input, &mut out);
        });
        assert_eq!(n, 0);
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
