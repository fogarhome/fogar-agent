# Testing long-run properties

The pyramid, fastest first: **paused-time unit tests**, then **deterministic simulation** (turmoil), then **loom** (only for custom primitives), then **compressed real-time churn soak** (CI, ≥2 h), then the **days-long soak on a real Mac** (nightly, ≥72 h, with sleep/wake cycles).

## 1. Paused time (tokio `test-util`)

`#[tokio::test(start_paused = true)]`: the clock auto-advances when every task is idle, so hours of timers run in milliseconds. E6 [exp] (`examples/tests/paused_time.rs`): a peer healthy for 6 virtual hours then silent. The heartbeat loop detects half-open at virtual 21,680 s after 1084 pings, in **8.8 ms wall**. Rules:
- All timing goes through tokio time or the `Clock` port. A `std::time::Instant`/`SystemTime` in core code defeats paused time, so a lint or review must catch it.
- Use `tokio::time::advance(d)` for precise stepping, and inject wall-clock jumps through a fake Clock.
- Assert **bounds** (detected within [idle, idle+ping]), not exact instants.
- Property: the backoff ceiling never exceeds the cap and never overflows at large attempt counts (`backoff_bounds` test).

## 2. Deterministic network simulation (turmoil 0.7.2)

turmoil runs hosts in one thread with simulated time and network. It offers `partition`/`repair`/`hold`/`release`/`crash`/`bounce`, `fail_rate`, latency ranges and `rng_seed` [primary, turmoil 0.7.2 `sim.rs`, `builder.rs`]. Its `turmoil::net::TcpStream` implements tokio's `AsyncRead`/`AsyncWrite` [primary], so `tokio_tungstenite::client_async(url, stream)` runs over it **unchanged**.

E10 [exp] (`examples/tests/turmoil_ws.rs`): the agent connects, the network is partitioned at t=60 s, half-open is detected at t=75.0 s (IDLE 15 s), connect attempts fail during the partition with backoff 0.5, 1, 2, 4, 8, 16 s, repair happens at 120 s, the agent reconnects, and the session stays healthy for 60 s. 182 simulated seconds run in 0.61 s wall.
Design implication (hexagonal): the WS adapter takes a **connector** (`async fn connect(&self, addr) -> io::Result<S> where S: AsyncRead + AsyncWrite`) so tests inject turmoil and production injects TCP + TLS.
Note: the turmoil main branch is being split into `turmoil-net`, `turmoil-fs` and other crates (0.1.0 on crates.io). Pin 0.7.2 and re-check before upgrading [primary: repo README, crates.io].

## 3. loom 0.7.3

Use loom only for hand-written lock-free or atomic code (the audio SPSC ring, a custom coalescing slot). It exhaustively permutes thread interleavings under the C11 memory model, which makes it expensive. Keep models tiny (2–3 threads, a few operations). Not needed for code that only composes tokio primitives [secondary].

## 4. Fault injection behind ports

Fakes implement the same port traits:
- `GatewayTransport`: scripted peer (close, RST, silence, slow reads, garbage frames, oversized frames, 401 at handshake, duplicate or out-of-order acks).
- `Storage`: inject `SQLITE_BUSY`, `SQLITE_FULL`, `SQLITE_CORRUPT` at open, and slow commits (500 ms).
- `Clock`: wall jumps of ±1 h and simulated sleeps.
- `SecretStore`: unavailable or locked, and rotation write failure.
- `PowerEvents`/`NetworkMonitor`: wake, path change and flapping (10 changes per second).
Each fault has an expected **health state** and an expected **recovery**, both asserted.

## 5. Compressed real-time churn soak

E5 [exp] (`examples/src/bin/e5_ws_soak.rs`, CSVs in `examples/`): a local hostile WS server picks a random mode per connection (polite close / TCP drop / silence) with a 0.3–3 s life. Load is 200 msgs/s through a bounded queue with shed counting. Samples every 5 s:

| run (120 s) | sessions | half-open | RSS KiB start → end | fds | alive tasks |
|---|---|---|---|---|---|
| healthy | 63 | 14 | 3256 → 4424 (flat after 80 s) | 11–15 | 3–6 |
| `LEAK=1` | 63 | 19 | 3348 → **7276** | 11–15 | 3 → **68** |

Make the leak canary mode a permanent part of the soak harness. It proves the probes and thresholds actually fire.

## 6. Days-long soak design (per release, per vertical slice)

**Rig**: a dedicated MacBook Air M2 on battery and on AC, plus a CI Linux runner. The gateway is either the real staging gateway or a chaos proxy (toxiproxy-style) in front of it. Run the agent binary in release with `panic=abort`, launched by launchd exactly as in production.

**Load model**: the realistic day compressed 10× (state changes, conversations, uploads), plus a background "evil hour" every 6 h: network flaps every 30 s, gateway restarts, 401s, slow gateway (5 s acks), disk at 95%.

**Sleep/wake**: `sudo pmset relative wake 120` followed by `pmset sleepnow` in a loop (for example every 30 min), plus lid-closed periods of 1–8 h, plus Wi-Fi toggling (`networksetup -setairportpower`) [secondary: macOS CLI tools; verify on the rig].

**Sampling**: the agent's own ops metrics, plus external `footprint`/`ps -o rss,%cpu`/`lsof -p | wc -l` every 10 min, plus the WAL and log directory size.

**Pass/fail (after a 1 h warm-up, 72 h minimum)**:
- RSS/phys_footprint slope < 1%/h, and the last 24 h max is less than 1.25 × the hour-1 max.
- fds and alive tasks return to baseline ±2 whenever idle and connected.
- Queue lengths are 0 at idle, and the high-water mark is below capacity outside the evil hours.
- CPU at idle (connected, wake word listening) is within the budget measured on the rig (target < 2% of one core for the wake word, < 0.2% for the rest); no busy loop (no 100% core for more than 1 s).
- Reconnect after every wake within 10 s of network availability. Half-open detected within `idle_deadline + ping`.
- Zero lost must-deliver messages: reconcile the outbox seqs with the gateway's received log. Zero duplicates applied (idempotency check).
- The watchdog never trips. No crash records. Graceful SIGTERM takes < 5 s with the WAL truncated.
- Log directory below its cap, `dropped_lines` explained by the evil hours only.

## Per-slice acceptance tests

| Slice | Paused-time / sim test | Soak assertion |
|---|---|---|
| Gateway connection & state sync | 24 h virtual: heartbeats, refresh at 80% TTL, half-open, backoff ceiling, wake reconnect. turmoil partition, repair and server bounce | sessions ≈ disconnect events, tasks/fds flat, the snapshot after every reconnect equals the server view |
| Conversation/session engine + SQLite | Retention and checkpoint cadence over 30 virtual days. Injected BUSY/FULL/CORRUPT | WAL < 64 MiB always, DB size bounded by the retention policy, `quick_check` ok at the end, outbox drained |
| Wake word / audio | Ring-buffer overflow accounting (loom on the SPSC ring). Helper-process crash and restart with backoff | no audio-callback overruns from our side, helper restarts counted, CPU budget held on battery, mic released on sleep and reacquired on wake |
| Uploads | Admission control: N+1th upload deferred. Chunk resume after disconnect | bytes in flight ≤ cap, no orphaned temp files, retries bounded |
| SSH sessions | Max-sessions limit, idle timeout, cancel on shutdown, PTY reader thread joins | fds back to baseline after each session, no zombie child processes |
| Config / secrets / auth | Invalid config keeps the old one, keychain unavailable then degraded then recovered, refresh token rotation | no secret in logs (grep the token canary), auth failures do not hot-loop |
| Supervision / ops | Crash-loop breaker transitions, watchdog trip on an injected stall | after a planted `panic!`, launchd restarts within 10–20 s, crash record uploaded, SafeMode after 4 crashes |
