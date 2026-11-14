---
name: fogar-longrun-reliability
description: Long-running reliability rules for the fogar-agent async Rust daemon (tokio, macOS-first, later Windows/Linux). Use when writing or reviewing any code that spawns tasks, uses select!, channels, spawn_blocking, timers/Instant/SystemTime, WebSocket reconnect/heartbeat/backoff/resume, SQLite (rusqlite, WAL, checkpoint, migrations, backup), panics/panic hooks/panic=abort, launchd/systemd/Windows service supervision, crash loops, watchdogs/health, memory or fd leaks, tokio metrics/console, tracing/log rotation, config reload, secrets, sleep/wake or network change, or when designing soak tests, paused-time tests, turmoil simulations, or a vertical slice's days-long acceptance test.
---

# fogar-agent: long-running async Rust reliability

Goal: runs for weeks with **flat** memory, fds, tasks and queue depths; never spins; never blocks the runtime; survives sleep/wake, network loss, gateway restarts and its own bugs. Versions checked 2026-11-12: tokio 1.54.1 (MSRV 1.85), tokio-util 0.7.20, tokio-tungstenite/tungstenite 0.30.0, rusqlite 0.42.0 (bundled SQLite 3.54.1), sqlx 0.9.0, tracing-appender 0.2.5, tokio-metrics 0.5.2, turmoil 0.7.2, loom 0.7.3. Re-check before pinning. Deep dives are in `references/`; compiled and executed examples are in `references/examples/`.

## Hard rules (a violation is a bug)

1. **Every task has an owner.** Spawn only through a `TaskTracker` (long-lived services) or a `JoinSet` that is drained (`join_next`) in the same loop. Never `tokio::spawn` and drop the handle in core code. Every task takes a `CancellationToken` (a child of its owner's token). Per-session helpers die with their session (`AbortOnDropHandle` or JoinSet drop).
2. **Every queue is bounded** and has a written full-policy (table below). No `unbounded_channel`, no unbounded `Vec`/`VecDeque`/`HashMap` growth keyed by external input, and no default `tungstenite` `max_write_buffer_size` (it is `usize::MAX`).
3. **Every await on the outside world has a deadline**: connect, TLS, WS handshake, each write, the close handshake, HTTP, token refresh, DB calls from async code, and each shutdown phase. tokio-tungstenite has no connect timeout of its own, so wrap it in `tokio::time::timeout`.
4. **Only cancel-safe futures as `select!` branches in loops**: `mpsc::Receiver::recv`, `watch::changed`, `StreamExt::next`, `CancellationToken::cancelled`, `sleep`/`interval.tick`. Never `Sender::send(v)` with a value you already took (use `reserve()` then `permit.send(v)`). Never `read_exact`, `write_all`, `Mutex::lock`, `Semaphore::acquire` or `Notify::notified` as a looping branch. Do writes inside the branch body, under a timeout.
5. **No blocking on runtime workers.** No `std::thread::sleep`, blocking file or SQLite I/O, CPU work over about 1 ms, or blocking FFI. Use a dedicated OS thread (SQLite writer, audio/wake word) with a channel, or `spawn_blocking` behind a `Semaphore`. The default blocking pool is 512 threads and its queue is unbounded, and a running blocking task cannot be aborted.
6. **Monotonic time for durations, wall time only for wall-clock facts** (token `exp`, log stamps, schedules). On macOS, `Instant` is `CLOCK_UPTIME_RAW` and **stops while asleep**. On Windows it keeps counting through sleep. Detect sleep with a wall-vs-monotonic jump detector (Clock port), and on a jump force a heartbeat check and a reconnect.
7. **Every interval sets `MissedTickBehavior::Delay` or `Skip`.** The default is `Burst`.
8. **Release builds use `panic = "abort"`**, with a synchronous crash-record panic hook (policy below).
9. **One SQLite writer** on its own OS thread. Write transactions are `BEGIN IMMEDIATE` (they honor `busy_timeout`), read transactions stay short, and a periodic checkpoint and `PRAGMA optimize` run.
10. **Logging never blocks and never grows without limit.** Use a non-blocking lossy writer with a small line bound, rotation with `max_log_files`, an exported dropped-lines counter, and no log per frame or per audio chunk on hot paths.
11. **Secrets live in `SecretString`** (redacted `Debug`). They are loaded from the SecretStore port and never logged or put in URLs.
12. **Every slice ships a soak test** (below) before it counts as done.

## Backpressure: what to do when a queue is full

| Data | Policy | Primitive |
|---|---|---|
| State snapshots (presence, device state) | **Coalesce**: the latest value wins | `watch::Sender::send_replace`, or a keyed map holding the latest value per key |
| Commands that must not be lost (user turns, acks) | **Block the producer** with a deadline, and persist first (durable outbox) | `reserve()` + timeout; SQLite outbox |
| Telemetry, metrics samples | **Drop oldest** and count drops | ring buffer (`DropOldest`) |
| Best-effort events (UI hints, debug) | **Shed newest** and count | `try_send` → `Full` → `counter += 1` |
| Audio frames to the wake-word engine | **Drop oldest, never block the audio callback** | fixed SPSC ring on an OS thread |
| Uploads (audio, image) | **Admission control**: at most N in flight, reject or defer the rest | `Semaphore::try_acquire`, chunked |

Export queue length (`Receiver::len()`), capacity and drop counters as metrics. A queue that sits at capacity means overload. Shed load there; do not grow the queue.

## Shutdown ordering (SIGTERM from launchd, then SIGKILL after ExitTimeOut)

1. Stop intake: cancel the `ingress` token (wake word, local API, timers that create work).
2. Drain: producers drop their `Sender`s, and the writer drains the channel, commits and runs `wal_checkpoint(TRUNCATE)`. Wait on `data_plane.wait()` under a timeout.
3. Say goodbye: cancel `egress`, send the WS Close frame (write under a timeout), and wait briefly for the server's TCP close. Wait on `io_plane.wait()` under a timeout.
4. Flush logs: drop the `WorkerGuard`. Then call `Runtime::shutdown_timeout(≤1s)`, because a stuck `spawn_blocking` would otherwise hold the exit forever.
5. Mark the boot ledger `clean` and exit 0. The total budget must be less than ExitTimeOut (aim for under 5 s).

Each phase gets its own `TaskTracker` and its own deadline. E2 ran this ordering and stayed under 300 ms even with a stuck blocking task present.

## WebSocket connection supervisor (shape)

- **Supervisor loop** (one task, owns the outbound `Receiver` across sessions): connect with a timeout, run the session, classify the end, then wait out the backoff (cancellable). **Backoff**: capped exponential with **full jitter**. The first retry is random too (RFC 6455 §7.2.3: random first delay and growing delays). Reset the attempt counter only after the session stayed healthy for a while (anti-flap). Skip the backoff, while keeping a small jitter, when a network-change or wake event arrives.
- **Session loop**: `select!` with `biased` over cancel, `stream.next()` (any inbound frame refreshes `last_rx`), `ping.tick()` (if `last_rx.elapsed() > idle_deadline` return HalfOpen, else send a Ping under a timeout), and `out_rx.recv()` (write under a timeout). Set idle_deadline to about 3 ping periods. tungstenite answers Pings automatically, but only while you keep reading.
- **Application heartbeat and resume**: on connect, send `hello{client_id, epoch, last_seen_server_seq, last_acked_client_seq}`. The server replays or sends a snapshot. The client replays its unacked durable outbox. All messages are idempotent (sequence numbers or idempotency keys). State messages are versioned snapshots, not deltas, when in doubt.
- **Auth**: refresh proactively (at about 80% of the TTL, compared against wall time), with single-flight refresh through a `watch`. A 401/403 at the handshake means one refresh, then retry. A second failure is an Auth-fatal state (report it; do not hot-loop).
- **Large payloads** never share a frame with control traffic. Chunk them (≤64 KiB), keep in-flight chunks bounded, and give pings and acks priority.

## Panic policy (firm)

**Release profile: `panic = "abort"`. Let launchd (KeepAlive) restart the process, and protect against crash loops inside the app.** Reasons: (a) a panic is a broken invariant, and with unwind tokio silently turns it into a `JoinError` that nobody may be holding (E3/E7), leaving half-dead state such as a closed channel or a missing service; (b) SQLite in WAL mode plus the durable outbox already make restart-from-disk correct, so a restart is the cheapest consistent recovery; (c) since Rust 1.81, unwinding out of `extern "C"` aborts anyway, so FFI callbacks (CoreAudio, IOKit) cannot rely on unwind. Requirements:
- Set the panic hook first thing in `main`. It writes the message, location, thread, uptime and a backtrace **synchronously** to `crash/last.txt` and fsyncs it. Do not use the non-blocking logger, whose buffer dies with the process. The next boot uploads the record.
- **Crash-loop breaker**: a boot ledger (E11). Four or more unclean starts in 10 minutes means **SafeMode**: an exponential start delay (1 min up to about 32 min), optional slices (wake word, SSH, uploads) disabled, and the agent connects only to report `crashloop` health. launchd only throttles respawns (10 s ThrottleInterval) and never gives up.
- Configuration or auth errors **do not exit**. Exiting makes launchd respawn in a loop. Stay up degraded, report health, and watch for a fix.
- `catch_unwind` is used only in tests and in non-abort builds. Untrusted or crashy native code (a C++ wake-word model) goes in a **separate helper process** behind a port, not inside `catch_unwind`.

## Watchdog and health

- An **in-runtime heartbeat** task stamps an `AtomicU64` every 1 s. An **OS-thread watchdog** (outside tokio) checks its age. If the stamp is stale by more than 5 s, the runtime is starved or deadlocked: dump metrics, write a crash record, then `abort()`. E3 showed the in-runtime probe can only report after the stall ends, while the OS thread sees it during the stall.
- Health is a struct with fields per slice (`ws: Connected|Backoff(n)|AuthFailed`, `db_last_commit_age`, `queue_depths`, `drops`, `rss`, `fds`, `tasks`, `boot_mode`). It goes into heartbeats to the gateway and is logged every 60 s at info level.

## Leak and growth probes (always on, cheap)

Sample every 60 s and export: `RuntimeMetrics::num_alive_tasks()`, `global_queue_depth()` and `worker_total_busy_duration` (all stable, no `tokio_unstable` needed), channel `len()` values, open fds, RSS or phys_footprint, jemalloc `stats::allocated`/`resident` (if jemalloc is used), SQLite WAL size, and log drop count. **Alert on a slope, not a level.** In the E5 leak run, alive tasks rose 3 → 68 across 63 reconnects while RSS crept from 3.3 to 7.3 MB. The healthy run stayed flat at 3–6 tasks and about 4.4 MB. Keep tokio-console, `tokio_unstable` hooks and taskdump (Linux-only) for dev builds.

## Soak acceptance checklist (per vertical slice)

- [ ] **Paused-time test** (`#[tokio::test(start_paused = true)]`) covering ≥24 virtual hours of the slice's timers: heartbeats, refresh, retention, checkpoint cadence. E6 ran 6 h of virtual time in 9 ms.
- [ ] **Fault-injection test** (turmoil 0.7.2, or fakes behind ports): partition, half-open, server restart, slow peer, auth expiry, disk-full/`SQLITE_FULL`, clock jump. E10 partitions the network, detects the half-open link at +15 s and reconnects after repair.
- [ ] **Compressed real-time churn soak** (≥2 h in CI, ≥72 h nightly on a Mac): a hostile peer (drop, silence, close, garbage), 10–100× normal event rates, and sleep/wake cycles (`pmset relative wake` on a test Mac).
- [ ] **Pass criteria** after warm-up: slopes of RSS, fds, alive tasks, queue depths and WAL size are within noise (for example RSS growth < 1% per hour after hour 1, and tasks and fds back to their baseline between sessions). No stale-heartbeat watchdog trips. p99 event loop lag < 50 ms. CPU while idle < 1% of a core. Zero lost must-deliver messages (outbox reconciliation). Clean shutdown under SIGTERM in < 5 s.
- [ ] **Leak canary**: the soak harness has a `LEAK=1` mode proving its probes catch a planted leak.

## Hexagonal ports (core never touches OS/time/IO directly)

`Clock` (monotonic now, wall now, sleep, plus a suspend/jump event stream), `PowerEvents` (will sleep, did wake), `NetworkMonitor` (path changed), `SecretStore`, `Storage`/`Outbox` (SQLite adapter), `GatewayTransport` (WS adapter, generic over `AsyncRead + AsyncWrite` so turmoil or a mock can be injected), `AudioSource`/`WakeWord`, `Supervisor` (crash ledger, notify/watchdog: launchd, systemd `sd_notify`, Windows SCM), `HealthSink`, `ConfigSource` (watch-based). Adapters per OS: see `references/ports-and-platforms.md`.

## References

- `references/structured-concurrency.md`: ownership, tokens, trackers, select! cancel safety, spawn_blocking, stall detection
- `references/backpressure.md`: patterns and code
- `references/websocket-client.md`: crate choice, config, supervisor and session, TLS, auth, resume, sleep/wake, network change
- `references/sqlite.md`: rusqlite vs sqlx, writer actor, pragmas, busy, checkpoint starvation, vacuum, backup, migrations, corruption
- `references/panic-supervision.md`: panic hook, launchd plist, crash loop, watchdog, systemd/Windows
- `references/observability.md`: metrics, leak probes, dhat/jemalloc, tracing config, cost
- `references/time-config-secrets.md`: clocks across sleep, jump detector, config reload, secrets, error taxonomy
- `references/testing-soak.md`: paused time, turmoil, loom, fault injection, soak design, measured results
- `references/ports-and-platforms.md`: port list, macOS/Windows/Linux differences
- `RESEARCH.md`: sources, confidence labels, conflicts, open questions
