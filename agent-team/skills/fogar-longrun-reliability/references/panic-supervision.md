# Panic policy, supervision, crash loops, watchdog

## Decision: `panic = "abort"` in release, plus a synchronous crash record, launchd KeepAlive and an in-app crash-loop breaker

```toml
[profile.release]
panic = "abort"
debug = 1          # line tables for usable backtraces in crash records
```
(Cargo ignores `panic` for tests and benches [secondary], so `#[should_panic]` tests still work.)

### Why not unwind plus per-task recovery
1. A tokio task panic becomes a `JoinError` delivered only to whoever holds the `JoinHandle`. A detached task's panic is visible only through the hook, and the rest of the process keeps running with a missing component [exp E3/E7]. The tokio `Mutex` does not poison [primary], so shared state can be half-mutated with no signal. `UnhandledPanic::ShutdownRuntime` is unstable [primary].
2. Since Rust 1.81, "Abort on uncaught panics in `extern "C"` functions" [primary, RELEASES.md]. CoreAudio, IOKit and Security.framework callbacks abort on a panic regardless of the profile. Unwind-based isolation does not cover the riskiest code.
3. Restart-from-disk is already correct: SQLite WAL is durable across application crashes "regardless of the synchronous setting" [primary], and the outbox plus gateway resume protocol re-establish state. A fresh process is the only state guaranteed consistent.
4. It is simpler: there is one recovery path (restart), and it is exercised in tests.

Costs, and how to handle them: in-memory work since the last commit is lost (keep it small, commit often). Logs buffered in the non-blocking writer are lost, so the hook writes synchronously. A crash loop is possible, so use the breaker below.

E7 [exp], the same program under each profile:
```
unwind: hook ran; JoinError::is_panic() == true; process still alive; exit=0
abort : hook ran; "Aborted"; exit=134 (SIGABRT); crash record with backtrace present in the file
```

### Panic hook (first statement in `main`)
```rust
std::panic::set_hook(Box::new(|info| {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(crash_path()).unwrap();
    let bt = std::backtrace::Backtrace::force_capture();
    let _ = writeln!(f, "panic thread={:?} uptime={:?} {info}\n{bt}", std::thread::current().name(), uptime());
    let _ = f.sync_all();
}));
```
Keep it allocation-light and lock-free where possible. Do not call into tracing, since the subscriber may hold a lock on the panicking thread. Cap the crash file (for example truncate if it exceeds 1 MiB). On the next boot, upload it, then rotate.

### Where `catch_unwind` still belongs
- Only in builds where unwinding exists (tests, a debug "unwind" profile). With `panic=abort`, `catch_unwind` never catches anything.
- Crash-prone native code (the C/C++ wake-word engine, codecs) goes in a **helper process** behind the `WakeWord` port, connected by a pipe or XPC with a bounded queue. The parent supervises it (restart with backoff, crash ledger), so a segfault there never kills the agent.

## launchd (macOS) [primary: launchd.plist(5)]

- `KeepAlive`: "The value may be set to true to unconditionally keep the job alive." `SuccessfulExit`/`Crashed` sub-keys exist. **`NetworkState` "is no longer implemented"**.
- `ThrottleInterval`: "by default, jobs will not be spawned more than once every 10 seconds". launchd never stops retrying, so it provides no crash-loop protection.
- `ExitTimeOut`: SIGTERM, then SIGKILL after a "system-defined" default (commonly cited as 20 s [secondary]). Finish shutdown within 5 s.
- `ProcessType`: unspecified means "light resource limits ... throttling its CPU usage". `Background` is for work "not directly requested by the user". `Interactive` gets no limits. For a voice agent, start with `Standard`/unspecified and measure wake-word latency on battery [open question].
- `StandardOutPath`/`StandardErrorPath` files are **not rotated by launchd**, so keep stdout/stderr nearly silent and do real logging through the rotating appender.
- LaunchAgent (per user, `~/Library/LaunchAgents`) vs LaunchDaemon (system): microphone (TCC) permission and the login Keychain belong to the user session, so a wake-word agent is a **LaunchAgent** [secondary]. Manage it with `launchctl bootstrap gui/<uid> ...` / `bootout`.

Example (inert text, adapt paths):
```xml
<plist version="1.0"><dict>
  <key>Label</key><string>ai.fogar.agent</string>
  <key>ProgramArguments</key><array><string>/Applications/Fogar.app/Contents/MacOS/fogar-agent</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ExitTimeOut</key><integer>15</integer>
  <key>SoftResourceLimits</key><dict><key>NumberOfFiles</key><integer>1024</integer></dict>
  <key>StandardErrorPath</key><string>/Users/USER/Library/Logs/Fogar/stderr.log</string>
</dict></plist>
```
A low `NumberOfFiles` limit turns an fd leak into a fast, visible failure (`EMFILE`) instead of a slow one. Alarm at 70% of the limit.

## Crash-loop breaker (compiled and run: `examples/src/bin/e11_crashloop.rs`)

The boot ledger lines are `<unix_ms> start|clean|stable`, kept to a 10-minute window (which also bounds the file), and fsynced. A `start` without a later `clean`/`stable` counts as a crash. Four or more crashes in the window means `SafeMode{delay: 60 s · 2^k (cap 32 min)}`. [exp]: boots 0–3 Normal, boot 4 SafeMode(60 s), boot 5 SafeMode(120 s). In SafeMode: sleep the delay (cancellable by SIGTERM), start only the core plus the gateway connection, report `boot_mode=safe, recent_crashes=n`, upload crash records, and keep optional slices off until the operator or gateway clears it, or until 30 min of stable uptime. Write `stable` after 10 min of uptime and `clean` on graceful exit.

## Watchdog

- Internal: an OS-thread watchdog over the runtime heartbeat (E3). Stale for more than 5 s means a metrics snapshot plus a crash record. Stale for more than 30 s means `std::process::abort()`, and launchd restarts the process.
- Per-slice liveness: each slice service updates `last_progress` (an `AtomicU64` of monotonic ms) when it does work **or** when it idles legitimately. A health task flags slices with no progress for longer than their SLO.
- systemd (Linux): `Type=notify`, `WatchdogSec=30` with `sd_notify(WATCHDOG=1)` from the **same** heartbeat logic, never from a free-running timer thread (a free-running thread hides a stalled runtime). On a missed watchdog, systemd terminates the service "with SIGABRT" [primary, systemd.service]. Use `Restart=always`, `RestartSec`, and `RestartSteps`/`RestartMaxDelaySec` for growing delays (10 s, 20 s, 40 s and so on per the man page), and mind the `StartLimitIntervalSec`/`StartLimitBurst` rate limit. Crate: `sd-notify` 0.5.0.
- Windows: run as a service via `windows-service` 0.8.1 [primary: crates.io], with SCM recovery actions (restart after N ms, reset period). SCM failure actions trigger only when the process ends without reporting `SERVICE_STOPPED`, unless the "non-crash failures" flag is set [unverified]. A per-user voice agent on Windows may also need to be a per-user logon task or app rather than a service (session 0 isolation has no audio device or user context) [secondary].

## Error taxonomy (drives retry/exit decisions)

| Class | Examples | Action |
|---|---|---|
| Transient | I/O timeout, connection reset, DNS failure, 5xx, `SQLITE_BUSY` | Retry with backoff and jitter. Count it. Health stays `Degraded`. |
| Needs action, then retry | 401 (refresh token), network changed, woke from sleep | Run the action (refresh or reconnect), retry once without backoff. |
| Resource exhausted | `SQLITE_FULL`, `EMFILE`, queue full, memory high-water | Shed load and degrade the feature. Alert. Do not retry hot. |
| Config / permission | Invalid config, mic permission denied, keychain item missing | **Stay alive**, report health, watch for change. Never exit, because exiting makes launchd loop. |
| Protocol violation | Unknown message version, invalid frame | Drop the session, reconnect with backoff, report. With version skew, wait for an update. |
| Bug / invariant | `unreachable!`, index out of bounds | Panic, which aborts, then restart and run the crash-loop breaker. |

Implement it as `enum Retry { Now, Backoff, AfterAction(Action), Never }` returned by `fn classify(&Error) -> Retry` on each adapter's error type (thiserror 2.0.x). Core code matches on the classification and never on string messages.
