# Lock-free hand-off from the audio thread: rings, wake-ups, verification

## Candidates (versions checked 2026-11-12)

| Crate | Version | Model | RT-safe ops | Notes |
|---|---|---|---|---|
| `rtrb` | 0.4.1 (2026-10-25) | SPSC, fixed capacity, heap alloc once | `push`, `push_partial_slice`, `push_entire_slice`, `write_chunk_uninit`, `pop_partial_slice`, `read_chunk` | "lock-free and wait-free… All reading and writing functions return immediately"; full → error, never overwrites; **no blocking wait** ("no way for the reading thread to wait… other than trying repeatedly") [primary: crate docs]. Capacity is exact (4800 → 4800 accepted) [exp]. **Default choice.** |
| `ringbuf` | 0.6.0 (2026-10-30) | SPSC, `HeapRb`/`StaticRb`, `traits::*` | `try_push`, `push_slice`, `pop_slice`, `occupied_len` | 0.6 reworked ownership ("Exclusive" adapter); overwrite mode exists (don't use for audio capture). Fine alternative [primary] |
| `async-ringbuf` (ringbuf workspace) | — | async SPSC | — | push wakes an `AtomicWaker` (`self.read.wake()`) → waking a tokio task from the RT thread can take scheduler locks. **Not on the RT side** [primary: source] |
| `crossbeam-queue::ArrayQueue` | 0.3.14 | MPMC bounded | `push`/`pop` | lock-free, per-item CAS; more expensive per sample; ok for small messages, not samples |
| `triple_buffer` | 9.0.1 | latest-value | `write` | good for "latest level meter" style state, not for streams |
| `basedrop` | 0.1.3 | deferred dealloc | — | only needed if you ever send heap objects to the RT thread (we don't) |

## Ring sizing
`capacity = rate × seconds` mono f32. 48 kHz × 3 s = 144 000 floats ≈ 576 KB. Big enough to survive
a dsp-thread stall (QoS throttling, model page-in) without drops; small enough for the <50 MB target.
Drops are counted in `CaptureStats.frames_dropped` and reported by the supervisor.

## Waking the consumer without touching the RT thread
- rtrb offers no blocking wait. Options:
  1. **Timer-paced consumer (recommended)**: dsp thread `thread::park_timeout(20–80 ms)` (or a
     blocking `recv_timeout` on its control channel), then drains everything available. Zero work on
     the RT side; cost = 12–50 wake-ups/s on a utility-QoS thread. Aligns naturally with 80 ms
     openWakeWord frames.
  2. Signal from RT thread with a semaphore post (Mach `semaphore_signal`, futex wake). Commonly
     used in pro-audio code but it *is* a syscall [secondary]; forbidden by this skill's rules.
- Never spin/yield-poll: Apple: "eliminate spin locks and other spin-wait code… avoid
  `pthread_yield_np`" [primary: Apple Silicon tuning guide].

## Verifying RT-safety

1. **Counting global allocator in tests** (works in release, all platforms) — the experiment
   `RtSink` test asserted 0 allocations across 10 oversized callbacks and exact drop accounting
   (10 240 frames into a 4 800-frame ring → 4 800 in, 5 440 dropped) [exp].
2. **`assert_no_alloc` 1.1.2**: `#[global_allocator] static A: AllocDisabler = AllocDisabler;` and wrap
   the callback body in `assert_no_alloc(|| …)`. Default feature `disable_release` turns it into a
   no-op in release builds [primary: Cargo.toml + lib.rs]; use `default-features = false` and
   `warn_release`/`warn_debug` (then `violation_count()`) or run tests in debug.
3. **RTSan** (`rtsan-standalone` 0.3.0): `#[rtsan_standalone::nonblocking]` on the callback body
   function, run with `RTSAN_ENABLE=1`; supports "Linux, macOS and iOS"; reports intercepted
   `malloc`/`calloc`/locks/syscalls with a stack trace [primary: README]. Its build script downloads
   prebuilt libs by default (supply-chain: vendor or build locally with default features off).
   Use it on the M2 against the real cpal CoreAudio path — the only way to also catch syscalls.
4. **loom** for any custom atomic protocol (counters + flags are trivially fine; a hand-rolled ring
   or a generation/handshake is not). rtrb itself is not loom-instrumented and cannot be swapped onto
   loom atomics without forking — rely on its upstream tests.

## loom pattern (experiment, loom 0.7.3) [exp]

```rust
#[cfg(loom)] use loom::{cell::UnsafeCell, sync::{Arc, atomic::{AtomicUsize, Ordering}}};
#[cfg(not(loom))] use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
// ... SPSC: producer writes slot via UnsafeCell::with_mut, then tail.store(next, Release);
//     consumer: tail.load(Acquire), slot via UnsafeCell::with, head.store(.., Release)

#[cfg(all(test, loom))]
#[test]
fn spsc() {
    loom::model(|| {
        let (mut p, mut c) = ring(2);
        let t = loom::thread::spawn(move || (p.push(11), p.push(22)));
        let mut got = Vec::new();
        for _ in 0..3 {
            if let Some(v) = c.pop() { got.push(v); }
            loom::thread::yield_now();          // <-- REQUIRED, see below
        }
        let (a, b) = t.join().unwrap();
        while let Some(v) = c.pop() { got.push(v); }
        // assert FIFO of the successful pushes
    });
}
```
Run: `RUSTFLAGS="--cfg loom" LOOM_LOG=info cargo test --release` and declare the cfg in
`Cargo.toml` (`[lints.rust] unexpected_cfgs = { level = "warn", check-cfg = ['cfg(loom)'] }`,
`[target.'cfg(loom)'.dependencies] loom = "0.7.3"`).

Results:
- Correct (Release/Acquire) version: passes, **42 iterations**.
- Deliberately buggy (`tail.store(.., Relaxed)`): with `yield_now()` loom panics
  "Causality violation: Concurrent read and write accesses" ✔.
- **Without `yield_now()` in the polling loop loom explored 1 iteration and the buggy version
  passed.** Lesson: put `loom::thread::yield_now()` in every poll loop and assert the iteration count
  (`LOOM_LOG=info` prints "Completed in N iterations").
- loom's own docs: it cannot model all Relaxed reorderings ("certain concurrency bugs that loom cannot
  catch") [primary: loom lib.rs docs]. Use Release/Acquire for anything that publishes data; Relaxed
  only for pure statistics counters.

## Memory-ordering rules for our shared state
- `callbacks`, `frames_in`, `frames_dropped`: `fetch_add(Relaxed)` — statistics, no data published.
- `signals`: `fetch_or(Relaxed)` / `swap(0, Relaxed)` — a lost race just delays a rebuild by one tick.
- Anything that hands over buffers/config: Release on publish, Acquire on observe (rtrb does this
  internally).
