# RESEARCH: long-running reliability of async Rust daemons (fogar-agent)

Date: 2026-11-12. Method: shallow clones of the primary sources (read-only), official docs via the web, and small experiments compiled and run on Linux x86_64 (rustc 1.97.0). No downloaded code was executed. Only my own experiment code ran, against crates.io dependencies.

Labels: **[primary]** read in source or official documentation. **[exp]** measured here. **[secondary]** reputable third-party writing or well-established knowledge not re-read today. **[unverified]** plausible but not checked.

## Versions checked on crates.io (2026-11-12, max stable)

tokio 1.54.1 (2026-10-21, MSRV 1.85), tokio-util 0.7.20, tokio-stream 0.1.19, tokio-tungstenite 0.30.0 / tungstenite 0.30.0 (MSRV 1.85), tokio-websockets 0.13.3, fastwebsockets 0.10.0, async-tungstenite 0.35.0, rusqlite 0.42.0 (libsqlite3-sys 0.40.0, bundled SQLite 3.54.1), sqlx 0.9.0, rusqlite_migration 2.7.0, refinery 0.10.0, tracing 0.1.44, tracing-subscriber 0.3.23, tracing-appender 0.2.5, tokio-metrics 0.5.2, console-subscriber 0.5.0, turmoil 0.7.2 (turmoil-net 0.1.0, turmoil-fs 0.1.0), loom 0.7.3, dhat 0.3.3 (2024-02-04), tikv-jemallocator/-ctl 0.7.0, rustls 0.23.46, tokio-rustls 0.26.6, rustls-platform-verifier 0.7.1, socket2 0.6.5, keyring 4.2.0 (keyring-core 1.0.1), secrecy 0.10.3, zeroize 1.9.1, arc-swap 1.9.5, notify 8.2.2, sd-notify 0.5.0, windows-service 0.8.1, backon 1.6.0, backoff 0.4.0 (last release 2021), thiserror 2.0.21. Rust 1.100.0 is listed in RELEASES.md as released 2026-11-12.

## Sources

Primary (read in clones under scratchpad/src, each in its own directory):
- tokio @ tag tokio-1.54.1: `runtime/builder.rs`, `runtime/metrics/runtime.rs`, `task/blocking.rs`, `runtime/runtime.rs`, `macros/select.rs`, `sync/mpsc/bounded.rs`, `sync/mutex.rs`, `task/join_set.rs`, `runtime/task/error.rs`, `time/interval.rs`, `macros/cfg.rs`, CHANGELOG.md, Cargo.toml. tokio-util 0.7.20: `task/task_tracker.rs`, `sync/cancellation_token.rs`, `task/abort_on_drop.rs`, `future.rs`.
- tokio-rs/website: `content/tokio/topics/shutdown.md` (graceful shutdown guide).
- tungstenite-rs v0.30.0: `src/protocol/mod.rs`, `src/error.rs`, CHANGELOG. tokio-tungstenite v0.30.0: `src/connect.rs`, `src/lib.rs`, `src/tls.rs`, Cargo.toml, CHANGELOG.
- rusqlite v0.42.0: `src/busy.rs`, `src/wal.rs`, `src/lib.rs`, Cargo.toml features, `libsqlite3-sys/build.rs`, bundled `sqlite3.h` / `sqlite3.c`.
- sqlx v0.9.0: `sqlx-sqlite/src/connection/worker.rs`, `options/mod.rs`.
- tracing-appender 0.2.5: `src/non_blocking.rs`, `src/rolling.rs`, `src/rolling/builder.rs`. tokio-metrics 0.5.2 README and `src/task.rs`, `src/runtime.rs`. turmoil 0.7.2 crate source (`sim.rs`, `builder.rs`, `net/tcp/stream.rs`) and the repo main-branch README.
- Rust std (rust-lang/rust master, sparse): `library/std/src/time.rs`, `library/std/src/sys/time/unix.rs`, RELEASES.md.
- sqlite.org: wal.html, pragma.html, lang_transaction.html, lang_vacuum.html, howtocorrupt.html.
- RFC 6455 (rfc-editor.org): §5.5.2, §5.5.3, §7.1.1, §7.2.3.
- launchd.plist(5) (Xcode man page mirror at keith.github.io). systemd.service(5) (freedesktop.org, "latest" = systemd 262).
- keyring 4.2.0 README (crate source).

Secondary:
- Rippling engineering, "rust-suspend-time" (Windows QPC counts suspend; macOS does not; tokio timeouts after resume).
- AWS Architecture blog "Exponential Backoff and Jitter" (full jitter) [from memory, not re-fetched].
- General knowledge: Cargo ignores `panic` for tests, IOKit power APIs, NWPathMonitor, logind PrepareForSleep, Windows SCM power events, macOS `footprint`/`leaks`/`vmmap`, `pmset`, TCC per-user microphone permission, LaunchAgent vs LaunchDaemon keychain access.

## Experiments (code in `references/examples/`, all compiled clean; clippy shows one intentional warning)

| ID | What | Result |
|---|---|---|
| E1 | `send()` vs `reserve()` inside `select!` | send: 1613/2000 **lost**; reserve: 0 lost [exp] |
| E2 | Phased shutdown with 2 TaskTrackers + tokens + stuck spawn_blocking | 280 ms ordered drain; `shutdown_timeout(500ms)` bounds the stuck blocking thread [exp] |
| E3 | Executor stall detection, blocking pool saturation, detached panic | OS-thread watchdog detected the stall at 499 ms during the stall; the in-runtime probe only afterwards (950 ms lag); the 3rd job on a 2-thread pool waited 500 ms; the detached panic was visible only via the hook [exp] |
| E4 | SQLite deferred upgrade, checkpoint starvation, journal_size_limit | DEFERRED upgrade → SQLITE_BUSY_SNAPSHOT (517) in 4.5 µs despite busy_timeout=2 s; IMMEDIATE waited 329 ms and succeeded; WAL grew to 78 MiB with a pinned reader and stayed that size after; TRUNCATE checkpoint → 0; with journal_size_limit=4 MiB, the WAL returned to 4096 KiB via autocheckpoint [exp] |
| E5 | 120 s hostile-server WS soak, healthy vs LEAK=1 | healthy flat (tasks 3–6, RSS ≈4.4 MiB); leak: tasks 3→68, RSS 3.3→7.3 MiB [exp] |
| E6 | Paused-time heartbeat (6 virtual hours) | 21,680 virtual s in 8.8 ms wall; half-open detected within bounds [exp] |
| E7 | panic=unwind vs abort with a sync hook | unwind: exit 0, JoinError::is_panic; abort: exit 134, crash record with backtrace written [exp] |
| E8 | Logging (rolling + non-blocking + reload), backpressure helpers, jump detector, ArcSwap, SecretString, jemalloc stats | all compiled and behaved as described; reload switched info→debug at runtime [exp] |
| E9 | rusqlite single-writer actor with group commit + migrations + VACUUM INTO | 10k appends ≈51 ms; user_version=2; WAL 0 bytes after shutdown [exp] |
| E10 | turmoil 0.7.2 partition/repair with tokio-tungstenite over a simulated TcpStream | half-open detected at +15 s; reconnect after repair; 182 sim-seconds in 0.61 s [exp] |
| E11 | Crash-loop breaker ledger | SafeMode from the 4th unclean start, delay doubles [exp] |

Limitations: every experiment ran on Linux, so **none of the macOS sleep/wake, launchd, Keychain, IOKit or fd-counting claims were executed**. Timings are from a VM and only indicative.

## Claims and confidence

### Tokio and concurrency
- The default `max_blocking_threads` is 512, keep-alive 10 s, and work beyond the limit queues; blocking tasks cannot be aborted once started; runtime shutdown waits for them indefinitely unless `shutdown_timeout` is used. [primary]
- `TaskTracker` frees tasks on exit; `JoinSet` retains results until joined; dropping a `TaskTracker` does not abort tasks; dropping a `JoinSet` aborts all of them. [primary]
- Cancel-safety lists for select! (safe: recv, changed, next, accept, read, write; unsafe: read_exact, write_all, Mutex::lock, Semaphore::acquire, Notify::notified; `send` drops its value if cancelled). [primary] Loss magnitude [exp E1].
- Stable runtime metrics: `num_workers`, `num_alive_tasks`, `global_queue_depth`, `worker_total_busy_duration`, `worker_park_count`, `worker_park_unpark_count`. Blocking-thread counts, histograms, task hooks (`on_task_spawn`, `on_before_task_poll` and others) and `unhandled_panic` need `tokio_unstable`. [primary]
- Task dump is Linux-only and needs `tokio_unstable` + `taskdump`. [primary]
- `MissedTickBehavior` defaults to `Burst`. [primary]
- The tokio `Mutex` does not poison. [primary]
- tokio 1.54.0 (2026-10-19) "remove the runtime handle from timers" was reverted in 1.54.1. [primary] (No impact identified.)

### WebSocket
- tungstenite `max_write_buffer_size` defaults to `usize::MAX`; message 64 MiB, frame 16 MiB, read/write buffers 128 KiB; pongs are queued and flushed on the next read/write/flush. [primary]
- tokio-tungstenite connect functions have no timeout; there is a `disable_nagle` flag; `Connector::Rustls(Arc<ClientConfig>)`; `client_async*` over generic streams. [primary]
- RFC 6455: Ping MAY be sent at any time; unsolicited Pong allowed; randomized first reconnect delay (0–5 s given as reasonable) and increasing delays; the client waits for the server's TCP close. [primary]
- Full jitter is the best default for reconnect storms. [secondary]
- rustls-platform-verifier use for macOS enterprise roots. [secondary] tokio-websockets / fastwebsockets suitability. [unverified]
- IOKit / NWPathMonitor / logind / SCM power-event mechanisms. [secondary]

### SQLite
- WAL facts: autocheckpoint at 1000 pages, unbounded WAL with overlapping readers, no truncation without journal_size_limit, WAL is persistent, no network filesystems. [primary] + [exp E4]
- A DEFERRED read→write upgrade fails with SQLITE_BUSY; the busy handler can be skipped to avoid deadlock. [primary] + [exp: extended code 517, immediate]
- rusqlite default busy timeout 5000 ms; bundled build sets `SQLITE_DEFAULT_FOREIGN_KEYS=1`; bundled SQLite 3.54.1. [primary]
- sqlx-sqlite uses one OS thread per connection with bounded flume channels (50/50). [primary]
- `PRAGMA optimize=0x10002` at open and periodic `optimize` for long-lived connections. [primary]
- auto_vacuum must be set before tables exist; VACUUM needs up to 2× space; VACUUM INTO gives a consistent snapshot but can be partial if interrupted. [primary]
- Corruption causes: copying open files, close() on DB fds, multiple SQLite copies, fork. [primary]
- macOS `fsync` vs `F_FULLFSYNC` semantics. [secondary]; SQLite uses F_FULLFSYNC only with the fullfsync pragmas. [primary, code comments]

### Panic and supervision
- Rust 1.81: abort on uncaught panics in `extern "C"`. [primary RELEASES.md]
- launchd: KeepAlive semantics, ThrottleInterval default 10 s, NetworkState no longer implemented, ExitTimeOut "system-defined", ProcessType semantics, AbandonProcessGroup. [primary man page] ExitTimeOut ≈20 s in practice. [secondary]
- systemd: WatchdogSec → SIGABRT; RestartSec default 100 ms; RestartSteps/RestartMaxDelaySec. [primary] StartLimit defaults not read. [unverified]
- Windows SCM recovery semantics and the non-crash-failure flag. [unverified]
- Cargo ignores `panic` for tests/benches. [secondary]

### Time
- macOS `Instant` = CLOCK_UPTIME_RAW, which stops in sleep; other Unix = CLOCK_MONOTONIC; Windows = QPC; std leaves suspend behavior unspecified. [primary] Windows QPC advances in sleep. [secondary] Linux CLOCK_MONOTONIC excludes suspend. [secondary]

### Logging
- tracing-appender: time-based rotation only, `max_log_files`, the non-blocking default is lossy with a 128,000-line limit, `error_counter()`. [primary]

## Conflicts recorded

1. **SQLite `synchronous=NORMAL` in WAL**: pragma.html says "WAL mode is safe from corruption with synchronous=NORMAL", while howtocorrupt.html says "For maximum reliability and for robustness against database corruption, SQLite should always be run with its default synchronous setting of FULL." Resolution for fogar: NORMAL for high-rate state (durability loss is bounded and resynced from the gateway), with end-to-end acks for must-deliver facts. Revisit if a corruption is ever observed in the field.
2. **Suspend semantics of `Instant`**: std documentation declines to specify. Implementation (macOS: does not advance) and secondary reports (Windows: advances) disagree across platforms. Resolution: never rely on it; use explicit power events + the jump detector.
3. **turmoil**: crates.io 0.7.2 has a single crate, while the repo main branch (2026-11-11) restructures into several crates. Pin 0.7.2.
4. **rusqlite FK default**: SQLite's own default is OFF ("might change"), but rusqlite's bundled build compiles it ON. Resolution: always set `PRAGMA foreign_keys` explicitly.

## Open questions (biggest unknowns)

1. **macOS sleep/wake reality on the M2**: DarkWake/Power Nap behavior for a LaunchAgent, how fast `NWPathMonitor` reports after wake, and whether IOKit notifications arrive reliably for an agent without a GUI run loop. Needs a rig test.
2. **Wake-word CPU vs ProcessType** on battery: `Standard` vs `Interactive` vs `Adaptive` latency and energy; App Nap interaction for a non-GUI agent.
3. **ExitTimeOut** actual default on current macOS, and whether `SIGABRT` exits count as `Crashed` for KeepAlive dictionaries (moot with `KeepAlive=true`).
4. **Allocator choice on macOS** (system malloc vs jemalloc vs mimalloc) for fragmentation over 7+ days with audio buffers.
5. **fd counting and phys_footprint on macOS** via `proc_pidinfo` / `task_info`. The APIs are known, but the code was not compiled here.
6. **checkpoint_fullfsync / fullfsync cost** on Apple SSDs at our commit rate.
7. **Gateway protocol**: whether the gateway can send server pings, a resume token or seq acks, since the resume design assumes it can.
8. **Windows**: SCM vs per-user process for microphone access; QPC-after-resume timer storms in tokio (only a secondary source).
9. **tokio-websockets/fastwebsockets** were not evaluated; revisit only if tungstenite shows a measured issue.
