//! The capture loop without the OS: drain the ring the RT callback fills, resample to 16 kHz,
//! cut 20 ms chunks, and decide when the capture ends. The stop signal, time and the permission
//! check are injected ([`Env`]), so every ending is table-tested on any host.

use super::dsp::{COUNTERS, Resampler};
use fogar_ports::{
    audio::AudioChunk,
    events::{EventSink, Pushed},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

/// Rate of the chunks the port delivers (spec S2: 16 kHz mono f32).
pub(crate) const CAPTURE_HZ: u32 = 16_000;
/// 20 ms at 16 kHz.
pub(crate) const CHUNK: usize = 320;
/// Time allowed for the FIRST input callback (Bluetooth/HFP routes such as AirPods take seconds
/// to switch profile).
pub(crate) const FIRST_CALLBACK_GRACE: Duration = Duration::from_secs(6);
/// After the first callback: no new callback for this long = stalled (sleep, HAL failure).
pub(crate) const STALL: Duration = Duration::from_secs(2);
/// How often the TCC permission is re-read while capturing (revocation ends the capture).
pub(crate) const PERMISSION_EVERY: Duration = Duration::from_secs(1);
/// Error-callback flag: the stream will deliver nothing more.
pub(crate) const SIG_FATAL: u32 = 1;
/// Most samples popped per resampler call.
const BLOCK: usize = 1_024;

/// Why a capture ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum End {
    /// The handle was dropped (normal end).
    Stopped,
    /// The core dropped its chunk stream.
    Closed,
    /// The device reported a fatal error (lost, invalidated).
    Fatal,
    /// No callback at all within [`FIRST_CALLBACK_GRACE`].
    NoFirstCallback,
    /// Callbacks stopped for longer than [`STALL`].
    Stalled,
    /// The microphone permission is no longer `Granted`.
    PermissionLost,
}

impl End {
    pub(crate) fn aborted(self) -> bool {
        !matches!(self, End::Stopped | End::Closed)
    }
}

/// Where chunks go (the core's bounded stream in production).
pub(crate) trait ChunkOut {
    fn push(&self, chunk: AudioChunk) -> Pushed;
}

impl ChunkOut for EventSink<AudioChunk> {
    fn push(&self, chunk: AudioChunk) -> Pushed {
        EventSink::push(self, chunk)
    }
}

/// What the loop needs from the outside world.
pub(crate) trait Env {
    /// Waits one tick (20 ms in production). `false`: stop requested (handle dropped).
    fn wait_tick(&mut self) -> bool;
    /// Monotonic time since the capture started.
    fn now(&mut self) -> Duration;
    /// The microphone permission is still `Granted`.
    fn permission_ok(&mut self) -> bool;
}

pub(crate) struct Pump {
    cons: rtrb::Consumer<f32>,
    resampler: Option<Resampler>,
    pending: Vec<f32>,
    heartbeat: Arc<AtomicU64>,
    signals: Arc<AtomicU32>,
    seen: u64,
    last_beat: Option<Duration>,
    last_permission: Duration,
}

impl Pump {
    /// `None` when `device_hz` cannot be resampled to 16 kHz.
    pub(crate) fn new(
        cons: rtrb::Consumer<f32>,
        device_hz: u32,
        heartbeat: Arc<AtomicU64>,
        signals: Arc<AtomicU32>,
    ) -> Option<Self> {
        let resampler = if device_hz == CAPTURE_HZ {
            None
        } else {
            Some(Resampler::new(device_hz, CAPTURE_HZ, BLOCK)?)
        };
        // Everything the ring can hold, at 16 kHz, plus a chunk: allocated once.
        let ring = cons.buffer().capacity();
        let cap = ring.saturating_mul(CAPTURE_HZ as usize) / (device_hz.max(1) as usize);
        Some(Self {
            cons,
            resampler,
            pending: Vec::with_capacity(cap + CHUNK + 64),
            heartbeat,
            signals,
            seen: 0,
            last_beat: None,
            last_permission: Duration::ZERO,
        })
    }

    fn check(&mut self, now: Duration, env: &mut impl Env) -> Option<End> {
        if self.signals.load(Ordering::Relaxed) & SIG_FATAL != 0 {
            return Some(End::Fatal);
        }
        let beat = self.heartbeat.load(Ordering::Relaxed);
        if beat != self.seen {
            self.seen = beat;
            self.last_beat = Some(now);
        } else {
            match self.last_beat {
                None if now > FIRST_CALLBACK_GRACE => return Some(End::NoFirstCallback),
                Some(t) if now.saturating_sub(t) > STALL => return Some(End::Stalled),
                _ => {}
            }
        }
        if now.saturating_sub(self.last_permission) >= PERMISSION_EVERY {
            self.last_permission = now;
            if !env.permission_ok() {
                return Some(End::PermissionLost);
            }
        }
        None
    }

    fn drain(&mut self) {
        while self.cons.slots() > 0 {
            let n = self.cons.slots().min(BLOCK);
            let Ok(chunk) = self.cons.read_chunk(n) else {
                break;
            };
            let (a, b) = chunk.as_slices();
            for part in [a, b] {
                match self.resampler.as_mut() {
                    Some(r) => r.process(part, &mut self.pending),
                    None => self.pending.extend_from_slice(part),
                }
            }
            chunk.commit_all();
        }
    }

    fn emit(&mut self, out: &impl ChunkOut) -> Option<End> {
        let mut sent = 0;
        let mut end = None;
        while self.pending.len() - sent >= CHUNK {
            let samples = self.pending[sent..sent + CHUNK].to_vec();
            sent += CHUNK;
            match out.push(AudioChunk { samples }) {
                Pushed::Delivered => {}
                Pushed::DroppedFull => {
                    COUNTERS.chunks_dropped.fetch_add(1, Ordering::Relaxed);
                }
                Pushed::Closed => {
                    end = Some(End::Closed);
                    break;
                }
            }
        }
        self.pending.drain(..sent);
        end
    }
}

/// Runs the capture until it ends. Drops `stream` (the device) BEFORE `out` (the core's chunk
/// stream), so the core sees the end only once the microphone is really off.
pub(crate) fn run<S, O: ChunkOut, E: Env>(mut pump: Pump, stream: S, out: O, env: &mut E) -> End {
    let end = loop {
        if !env.wait_tick() {
            break End::Stopped;
        }
        let now = env.now();
        if let Some(e) = pump.check(now, env) {
            break e;
        }
        pump.drain();
        if let Some(e) = pump.emit(&out) {
            break e;
        }
    };
    if end.aborted() {
        COUNTERS.captures_aborted.fetch_add(1, Ordering::Relaxed);
    }
    drop(stream);
    drop(out);
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};

    type Log = Rc<RefCell<Vec<&'static str>>>;

    /// Stands for the cpal stream: records when it is dropped.
    struct Device(Log);
    impl Drop for Device {
        fn drop(&mut self) {
            self.0.borrow_mut().push("device");
        }
    }

    /// Stands for the core's stream: records chunk sizes, answers with `answer`.
    struct Out {
        log: Log,
        sizes: Rc<RefCell<Vec<usize>>>,
        answer: Pushed,
    }
    impl ChunkOut for Out {
        fn push(&self, chunk: AudioChunk) -> Pushed {
            self.sizes.borrow_mut().push(chunk.samples.len());
            self.answer
        }
    }
    impl Drop for Out {
        fn drop(&mut self) {
            self.log.borrow_mut().push("sink");
        }
    }

    /// Scripted world: 20 ms ticks; `script(tick, world)` runs before each tick and returns
    /// whether the handle is still alive.
    struct World {
        prod: rtrb::Producer<f32>,
        beat: Arc<AtomicU64>,
        signals: Arc<AtomicU32>,
        permission: bool,
    }
    struct Fake<F: FnMut(u32, &mut World) -> bool> {
        tick: u32,
        world: World,
        script: F,
    }
    impl<F: FnMut(u32, &mut World) -> bool> Env for Fake<F> {
        fn wait_tick(&mut self) -> bool {
            self.tick += 1;
            (self.script)(self.tick, &mut self.world)
        }
        fn now(&mut self) -> Duration {
            Duration::from_millis(20 * u64::from(self.tick))
        }
        fn permission_ok(&mut self) -> bool {
            self.world.permission
        }
    }

    struct Outcome {
        end: End,
        ticks: u32,
        sizes: Vec<usize>,
        log: Vec<&'static str>,
    }

    fn simulate(
        device_hz: u32,
        answer: Pushed,
        script: impl FnMut(u32, &mut World) -> bool,
    ) -> Outcome {
        let (prod, cons) = rtrb::RingBuffer::<f32>::new(device_hz as usize * 2);
        let beat = Arc::new(AtomicU64::new(0));
        let signals = Arc::new(AtomicU32::new(0));
        let pump = Pump::new(cons, device_hz, beat.clone(), signals.clone()).unwrap();
        let log: Log = Rc::default();
        let sizes = Rc::new(RefCell::new(Vec::new()));
        let out = Out {
            log: log.clone(),
            sizes: sizes.clone(),
            answer,
        };
        let mut env = Fake {
            tick: 0,
            world: World {
                prod,
                beat,
                signals,
                permission: true,
            },
            script,
        };
        let end = run(pump, Device(log.clone()), out, &mut env);
        let ticks = env.tick;
        let sizes = sizes.borrow().clone();
        let log = log.borrow().clone();
        Outcome {
            end,
            ticks,
            sizes,
            log,
        }
    }

    /// One 10 ms device callback of a 48 kHz mic.
    fn callback(w: &mut World) {
        for _ in 0..480 {
            let _ = w.prod.push(0.25);
        }
        w.beat.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn endings_table() {
        // (name, device rate, core answer, script, expected end, tick at which it ends)
        type Script = Box<dyn FnMut(u32, &mut World) -> bool>;
        type Case = (
            &'static str,
            u32,
            Pushed,
            Script,
            End,
            std::ops::RangeInclusive<u32>,
        );
        let cases: Vec<Case> = vec![
            (
                "handle dropped",
                48_000,
                Pushed::Delivered,
                Box::new(|t, w| {
                    callback(w);
                    t < 10
                }),
                End::Stopped,
                10..=10,
            ),
            (
                "core dropped its stream",
                48_000,
                Pushed::Closed,
                Box::new(|_, w| {
                    callback(w);
                    callback(w);
                    true
                }),
                End::Closed,
                1..=3,
            ),
            (
                "fatal device error",
                48_000,
                Pushed::Delivered,
                Box::new(|t, w| {
                    callback(w);
                    if t == 5 {
                        w.signals.fetch_or(SIG_FATAL, Ordering::Relaxed);
                    }
                    true
                }),
                End::Fatal,
                5..=5,
            ),
            (
                // AirPods: first callback after 5 s is still fine, then normal.
                "slow first callback within grace",
                48_000,
                Pushed::Delivered,
                Box::new(|t, w| {
                    if t >= 250 {
                        callback(w);
                    }
                    t < 300
                }),
                End::Stopped,
                300..=300,
            ),
            (
                "no first callback",
                48_000,
                Pushed::Delivered,
                Box::new(|_, _| true),
                End::NoFirstCallback,
                301..=301,
            ),
            (
                "callbacks stop after starting",
                48_000,
                Pushed::Delivered,
                Box::new(|t, w| {
                    if t <= 50 {
                        callback(w);
                    }
                    true
                }),
                End::Stalled,
                151..=151,
            ),
            (
                "permission revoked",
                48_000,
                Pushed::Delivered,
                Box::new(|t, w| {
                    callback(w);
                    if t == 120 {
                        w.permission = false;
                    }
                    true
                }),
                End::PermissionLost,
                120..=150,
            ),
        ];
        for (name, hz, answer, script, end, at) in cases {
            let o = simulate(hz, answer, script);
            assert_eq!(o.end, end, "{name}");
            assert!(at.contains(&o.ticks), "{name}: ended at tick {}", o.ticks);
            assert_eq!(
                o.log,
                ["device", "sink"],
                "{name}: device closed before the sink"
            );
            assert!(o.sizes.iter().all(|&n| n == CHUNK), "{name}: chunk sizes");
        }
    }

    #[test]
    fn chunks_are_exactly_20_ms_at_16_khz_from_any_rate() {
        for hz in [16_000u32, 44_100, 48_000] {
            let per_tick = hz as usize / 50; // 20 ms of device audio
            let o = simulate(hz, Pushed::Delivered, |t, w| {
                for _ in 0..per_tick {
                    let _ = w.prod.push(0.1);
                }
                w.beat.fetch_add(1, Ordering::Relaxed);
                t < 100
            });
            assert_eq!(o.end, End::Stopped);
            assert!(o.sizes.iter().all(|&n| n == CHUNK), "{hz}: {:?}", o.sizes);
            // 2 s of audio = 100 chunks, minus the resampler's start-up delay.
            assert!(
                (97..=100).contains(&o.sizes.len()),
                "{hz}: {}",
                o.sizes.len()
            );
        }
    }

    #[test]
    fn a_full_core_stream_drops_and_counts_but_keeps_capturing() {
        let before = COUNTERS.chunks_dropped.load(Ordering::Relaxed);
        let o = simulate(48_000, Pushed::DroppedFull, |t, w| {
            callback(w);
            callback(w);
            t < 20
        });
        assert_eq!(o.end, End::Stopped);
        assert!(COUNTERS.chunks_dropped.load(Ordering::Relaxed) >= before + 15);
    }

    #[test]
    fn unsupported_device_rate_is_refused() {
        let (_, cons) = rtrb::RingBuffer::<f32>::new(16);
        let z = || Arc::new(AtomicU64::new(0));
        assert!(Pump::new(cons, 0, z(), Arc::new(AtomicU32::new(0))).is_none());
    }
}
