# tokio configuration for low idle cost (tokio 1.54.1, Nov 2026)

Sources: tokio master @ ab0814f (version 1.54.1, the latest on crates.io on 2026-11-12) —
`tokio/src/runtime/builder.rs`, `runtime/time/mod.rs`, `runtime/time/wheel/level.rs`, `time/interval.rs`,
`sync/mpsc/*`; mio 1.2.4 `sys/unix/selector/kqueue.rs`. Experiments: **Linux** (2 vCPU VM, kernel 6.18,
rustc 1.97.0), counting voluntary context switches per thread from `/proc/<pid>/task/*/status` and
`strace`. Linux results show tokio's own behaviour; macOS kqueue + XNU behaviour is **untested**.

## 1. Facts from the source [primary]

* `worker_threads` default = `TOKIO_WORKER_THREADS` env or `std::thread::available_parallelism()`
  (8 on an M2 Air: 4P+4E). `max_blocking_threads` default **512**; blocking threads exit after
  `thread_keep_alive` (default **10 s**) idle; blocking queue "does not apply any backpressure".
* `event_interval` default **61** ticks, `global_queue_interval` default 31 (current-thread). These are
  fairness/latency knobs under load; they do **not** change idle cost.
* `disable_lifo_slot()` is **unstable** (`--cfg tokio_unstable`) and documented as a throughput workaround
  for the non-stealable LIFO slot. Not an energy knob — do not use it for idle cost.
* Other unstable knobs seen in 1.54: `enable_alt_timer()`, `enable_eager_driver_handoff()`,
  `enable_sharded_blocking_queue()`. None are needed for idle efficiency; avoid unstable cfg in production.
* `on_thread_start` runs on every runtime thread (workers **and** blocking pool) → the right hook for QoS.
  `on_thread_park/unpark` exist (stable) → cheap counters for "how often do we wake".
* Time driver: resolution **1 ms**, hierarchical wheel of 6 levels × 64 slots (level 0 = 1 ms slots,
  level 1 = 64 ms, level 2 ≈ 4 s, ...). When idle the driver parks until `wheel.next_expiration_time()`,
  and with no timers it parks with **no timeout**. `next_expiration` returns the **start of the occupied slot
  at that timer's level**, so a far timer is reached by cascading: one wakeup per level crossed.
* `Interval` default `MissedTickBehavior::Burst` (fires missed ticks back-to-back after a stall/sleep).
  `Delay` restarts the period from now; `Skip` jumps to the next multiple. For a daemon that may be suspended
  by system sleep, use **Delay** (or Skip) to avoid a burst of catch-up work on wake.
* mio/kqueue: waker = `EVFILT_USER` + `NOTE_TRIGGER`; poll = `kevent(..., timespec)`.

## 2. Linux measurements (this research, 10 s windows after 2 s warm-up)

| Scenario | threads | wakeups/s | Notes |
|---|---|---|---|
| current_thread, idle (only a 1 h sleep pending) | 1 | **0.0** | |
| multi_thread default (2 workers here), idle | 3 | **0.0** | idle workers park without timeout |
| multi_thread worker_threads(4), idle | 5 | 0.0 | more threads ≠ more idle wakeups, only memory |
| task waiting on an mpsc that never receives | 1 | 0.0 | waiting on channels/sockets is free |
| 1 s `interval` (Delay) | 1 or 3 | **2.1** | 2 wakeups per tick (level-1 cascade + final) |
| 10 ms `interval` | 1–5 | ~115 | ~1.15 wakeups/tick regardless of flavor |
| one 30 s interval | 1 | 0.10 (ideal 0.033) | strace: 3 sleeps/tick: wake at 28.7 s, +1.25 s, +0.05 s |
| 10 staggered 30 s intervals | 1 | 0.90 (ideal 0.33) | cascade cost multiplies with distinct deadlines |
| 50 staggered 30 s intervals | 1 | **3.55** (ideal 1.67) | |
| 50 **aligned** 30 s intervals (same phase) | 1 | **0.10** | 35x fewer wakeups than staggered |
| TCP: 10 small msgs/s, read → mpsc → consumer task | 1–5 | **1.0 per message** | ct, mt(1), mt(2), mt(4) identical |

RSS (Linux): current_thread 2.7 MB, default multi-thread 2.9 MB, +4 workers ≈ +0.1 MB resident.

Reproduction source: `references/linux-tokio-idle-experiment.md`.

## 3. Rules that follow

1. **Idle must mean "no timer armed except the coarse heartbeat".** Every `interval`/`sleep` loop is a
   standing wakeup source; multiply by ~2–3 for cascading when deadlines are spread out.
2. **One heartbeat, aligned deadlines.** Run periodic housekeeping (keepalive check, metrics flush,
   lease renewal, GC of caches) from a single task, or round every far deadline up to a common grid
   (e.g. next multiple of 5 s or 30 s of a process-wide epoch) so they land in the same wheel slot.
   Linux showed 50 timers aligned = cost of 1.
3. **Event-driven over polling.** Waiting on a socket, channel, `Notify`, `watch`, fd (AsyncFd) costs zero
   wakeups. Polling files/APIs on a timer is forbidden in the hot design (allowed only behind a power-aware
   backoff, see `signals-and-load-shedding.md`).
4. **Timeouts are not free when they expire, free when they don't.** A per-request `tokio::time::timeout`
   that is usually satisfied costs only insert/remove. But per-connection idle timeouts that are *reset
   constantly* are fine; ones that *fire* constantly are wakeups.
5. **Runtime flavor.** Idle cost is identical; choose by failure mode and memory:
   * `new_current_thread()` + dedicated BG `std::thread`(s): fewest threads, lowest memory, a blocking bug
     stalls everything.
   * `new_multi_thread().worker_threads(2)`: tolerates one accidentally slow task; 2 extra stacks. Default
     (8 on M2) wastes memory and lets bursts spread across P cores — always set it explicitly.
6. **Blocking pool:** set `max_blocking_threads` low (e.g. 4–8) for a daemon; `tokio::fs` and DNS via
   `ToSocketAddrs` use it. Thread churn: each idle blocking thread exits after keep-alive (10 s) and is
   re-created on next use; keep keep-alive ≥ the typical gap between fs bursts, or batch fs work.
7. **Burst handling:** coalesce events before acting (e.g. debounce file-change or config events 100–500 ms
   with one `sleep` that is only armed while events are pending).

## 4. Channels and bounded queues

* Always bounded (`tokio::sync::mpsc::channel(n)`): tokio's own docs warn the unbounded variant is bounded
  only by system memory. Size `n` from the burst you accept; on full, apply the shed policy (drop oldest,
  coalesce, or backpressure the producer) — decide per queue and document it.
* State that only the latest value matters (power state, config, connection status) → `tokio::sync::watch`
  (one slot, no growth). Fan-out events → `broadcast` with a small capacity and explicit `Lagged` handling.
* mpsc memory is block-based and blocks are reused (`reclaim_block` retries pushing a freed block to the tail
  up to 3 times before freeing) — capacity is not preallocated, but a large burst leaves blocks allocated.

## 5. WebSocket keepalive (tungstenite / tokio-tungstenite 0.30.0)

* tungstenite answers pings automatically (queues the pong; it is flushed on the next read/write/flush), but
  sends **no** pings by itself → the app owns keepalive.
* Prefer, in order: (a) server-side pings (we just answer; zero timers of our own), (b) **one** idle timer that
  is pushed forward on any traffic and sends a ping only after ≥N s of silence (N = 30–60 s, or whatever the
  server/NAT requires — measure NAT timeout), (c) OS TCP keepalive (`socket2` `set_tcp_keepalive`) as dead-peer
  detection; it costs kernel work, not user wakeups [unverified on macOS].
* Never a fixed-rate ping regardless of traffic; never < 15 s without a documented reason.
* Memory: `WebSocketConfig.read_buffer_size` default **128 KiB eagerly allocated**, `write_buffer_size` 128 KiB,
  `max_message_size` 64 MiB, `max_frame_size` 16 MiB. For one or two control connections set the read buffer to
  8–16 KiB and the message cap to what the protocol needs (protects footprint against a bad peer).
* On reconnect use exponential backoff with jitter and a cap (e.g. 1 s → 5 min); on battery/thermal pressure,
  stretch the cap.

## 6. Instrumenting wakeups in-process (cheap)

`on_thread_unpark(|| UNPARKS.fetch_add(1, Relaxed))` gives a portable "runtime wakeups" counter; export
`unparks/s` next to OS counters (`ri_pkg_idle_wkups`, see measurement file). If the two disagree a lot, a
non-tokio thread (logging, a library's own thread) is waking.
