# Time, configuration reload, secrets

## Clocks across sleep [primary, Rust std `time.rs` + `sys/time/unix.rs`]

| OS | `Instant` source | Advances during system sleep? |
|---|---|---|
| macOS (Darwin) | `clock_gettime(CLOCK_UPTIME_RAW)` (= `mach_absolute_time`) | **No**. "does not increment while the system is asleep" |
| Linux | `clock_gettime(CLOCK_MONOTONIC)` | No (CLOCK_BOOTTIME would) [secondary] |
| Windows | `QueryPerformanceCounter` | **Yes** [secondary: Rippling blog; std says unspecified] |

std explicitly says "it is also not specified whether system suspends count as elapsed time or not. The behavior varies across platforms and Rust versions." Tokio's `Instant`, `sleep`, `interval` and `timeout` build on this clock [secondary for the tokio internals].

Consequences:
- macOS: a `timeout(30 s)` spanning a 2 h sleep fires about 30 s of **awake** time after wake. Nothing "times out" at wake, so liveness must be re-proven explicitly after wake.
- Windows: everything with a deadline may fire immediately at resume, which looks like a burst of failures. Classify post-wake failures as expected.
- `Interval` with the default `MissedTickBehavior::Burst` can fire a catch-up burst after a stall [primary: the default is Burst]. Always set `Delay` (heartbeats) or `Skip` (samplers).

## Clock port

```rust
pub trait Clock: Send + Sync + 'static {
    fn mono_now(&self) -> tokio::time::Instant;   // durations, deadlines
    fn wall_now(&self) -> std::time::SystemTime;  // token exp, schedules, log stamps
    fn events(&self) -> tokio::sync::watch::Receiver<ClockEpoch>; // bumps on wake / jump
}
```
The production adapter combines OS power notifications (macOS IOKit, Linux logind, Windows SCM power events) with the portable **jump detector** (compiled in E8):
```rust
struct JumpDetector { mono: Instant, wall: SystemTime }
fn check(&mut self, tolerance: Duration) -> Option<i128> {
    let m = self.mono.elapsed().as_millis() as i128;
    let w = match SystemTime::now().duration_since(self.wall) {
        Ok(d) => d.as_millis() as i128, Err(e) => -(e.duration().as_millis() as i128) };
    self.mono = Instant::now(); self.wall = SystemTime::now();
    let skew = w - m;                          // >0: slept (macOS/Linux) or clock stepped forward
    (skew.abs() > tolerance.as_millis() as i128).then_some(skew)
}
```
Run it every 5 s with a 2 s tolerance. On a hit, bump `ClockEpoch`. The WS session probes or reconnects, `AuthService` re-checks expiry, and schedulers recompute. On Windows, where `Instant` includes sleep, the detector sees no skew, so the SCM power event is required there.
Tests use the paused tokio clock for `mono_now`, plus a fake wall clock that the test can jump.

## Configuration reload (compiled: E8)

- Store config in `Arc<ArcSwap<Config>>` (arc-swap 1.9.5). Readers `load()` a snapshot per operation, and never hold a guard across long awaits. The watcher validates the **whole** new config first; an invalid file keeps the old config and reports health.
- Or use `watch::Sender<Arc<Config>>` when consumers must **react** (heartbeat period change: the session `select!` adds a `cfg_rx.changed()` branch and rebuilds its interval).
- Sources: file watch (notify 8.2.2) **with a debounce** (editors write twice), SIGHUP on Unix, a gateway command, and a periodic re-read every 5 min as a fallback, since FSEvents can coalesce or miss events [secondary].
- Fields are classified as **hot** (log filter via `reload::Handle`, heartbeat timings, feature flags) or **cold** (DB path, identity), where a cold change requires a graceful restart: exit 0 under KeepAlive. Never half-apply a config.

## Secrets (SecretStore port)

- In memory: `secrecy::SecretString` (0.10.3). `Debug` prints `SecretBox<str>([REDACTED])` [exp]. It is zeroized on drop (zeroize 1.9.1). Call `expose_secret()` only at the header-building site.
- At rest, on macOS: the Keychain. The `keyring` 4.2.0 crate is now a thin wrapper; its README tells applications to depend on `keyring-core` plus a specific store crate (`apple-native-keyring-store` 1.0.2, `windows-native-keyring-store` 1.1.0, `zbus-secret-service-keyring-store` 1.0.1) [primary, keyring 4.2.0 README / crates.io]. A LaunchAgent can reach the user's login keychain once the user has logged in. Before login (or as a LaunchDaemon) it cannot, so treat `SecretStore` unavailable as a **Config/permission** class error: stay up and retry on the session/unlock event [secondary].
- Never put secrets in: logs, panic messages (do not `unwrap()` on results that embed tokens in `Display`), URLs/query strings, crash records, SQLite (except an encrypted refresh token if unavoidable), or env vars passed to SSH children.
- Rotation: the refresh token is rotated by the server and written back to the SecretStore **before** the old one is discarded. If the write fails, keep using the in-memory token and report it.
