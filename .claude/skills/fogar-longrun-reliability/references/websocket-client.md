# WebSocket client robustness (GatewayTransport adapter)

## Crate choice (checked on crates.io 2026-11-12)

| Crate | Version | Notes |
|---|---|---|
| **tokio-tungstenite** | 0.30.0 (tungstenite 0.30.0, MSRV 1.85) | Recommended. Mature, ships an Autobahn client example, offers `client_async*` over **any** `AsyncRead + AsyncWrite` (which allows injecting turmoil, mocks or a custom socket). TLS via `native-tls`, `rustls-tls-native-roots` or `rustls-tls-webpki-roots` features [primary, Cargo.toml]. |
| tokio-websockets | 0.13.3 | Alternative. Not evaluated in depth [unverified]. |
| fastwebsockets | 0.10.0 | Lower-level and performance-oriented, built around hyper upgrades. Not evaluated [unverified]. |
| async-tungstenite | 0.35.0 | Runtime-agnostic wrapper over tungstenite [secondary]. |

For one long-lived connection, throughput is irrelevant. Correctness, timeouts and bounded buffers decide. Stay on tokio-tungstenite unless a measured problem appears.

### Facts that matter [primary: tungstenite 0.30 `protocol/mod.rs`, tokio-tungstenite `connect.rs`]
- `WebSocketConfig` is `#[non_exhaustive]`, with builder methods `read_buffer_size` (default 128 KiB), `write_buffer_size` (128 KiB), `max_write_buffer_size` (**`usize::MAX`**), `max_message_size` (64 MiB), `max_frame_size` (16 MiB) and `accept_unmasked_frames`.
- Pings are auto-answered: "upon receiving ping messages tungstenite queues pong replies automatically. The next call to read, write or flush will write & flush the pong reply." So **a client that stops reading stops answering pings**. E10 relies on this for half-open detection.
- `connect_async*` has **no timeout parameter**. Wrap it. `disable_nagle: bool` is a parameter. Use `true` for small interactive frames.
- RFC 6455 [primary]: "An endpoint MAY send a Ping frame any time after the connection is established"; "A Pong frame MAY be sent unsolicited" (a unidirectional heartbeat); and on abnormal closure "The first reconnect attempt SHOULD be delayed by a random amount of time ... subsequent reconnect attempts SHOULD be delayed by increasingly longer amounts of time" (§7.2.3). The client "SHOULD wait for a TCP Close from the server" (§7.1.1), so put a 1–2 s timeout on that wait.

## Supervisor + session (compiled and soaked: `examples/src/bin/e5_ws_soak.rs`)

Key points of the code:
```rust
let cfg = WebSocketConfig::default()
    .max_message_size(Some(1 << 20)).max_frame_size(Some(1 << 20))
    .write_buffer_size(64 * 1024).max_write_buffer_size(1 << 20);
let (ws, _) = timeout(CONNECT_TIMEOUT,
    tokio_tungstenite::connect_async_with_config(url, Some(cfg), true)).await??;
let (mut sink, mut stream) = ws.split();
loop {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => { let _ = timeout(WRITE_TIMEOUT, sink.send(Message::Close(None))).await; return Ok(End::Cancelled); }
        m = stream.next() => match m {                  // cancel-safe
            Some(Ok(Message::Close(_))) | None => return Ok(End::RemoteClosed),
            Some(Ok(_)) => last_rx = Instant::now(),   // ANY frame proves liveness
            Some(Err(_)) => return Ok(End::Error),
        },
        _ = ping.tick() => {
            if last_rx.elapsed() > IDLE_DEADLINE { return Ok(End::HalfOpen); }
            timeout(WRITE_TIMEOUT, sink.send(Message::Ping(nonce_bytes.into()))).await??;
        }
        msg = out_rx.recv() => { /* write under timeout; receiver outlives sessions */ }
    }
}
```
Backoff is capped exponential with **full jitter** (`rand(0..=min(cap, base·2^n))`, with a small floor). Reset it only after the session lasted longer than the stable threshold. Suggested production values: base 500 ms, cap 60 s, ping every 15–20 s, idle deadline 45–60 s, connect timeout 10 s, write timeout 10 s. The AWS "Exponential Backoff and Jitter" analysis favors full jitter for contention [secondary].

Soak result, 120 s, hostile server that randomly closes, drops TCP or goes silent [exp]:
- healthy: 63 sessions, 14 half-open detections, RSS 3.2 → 4.4 MiB (flat after 80 s), fds 11–15, alive tasks 3–6, 24,032 messages sent, 0 shed.
- `LEAK=1` (a detached per-session task): alive tasks 3 → **68** and RSS 3.3 → **7.3 MiB**, linear in sessions. The task-count probe catches the leak first.

## Half-open detection layers

1. **Application idle deadline** (above): authoritative, portable, and fast enough.
2. Server-sent heartbeats: if the gateway pings or sends app heartbeats, any inbound frame refreshes `last_rx`.
3. TCP keepalive (socket2 0.6.5 `TcpKeepalive`) as a backstop for idle sockets [secondary]. That requires building the `TcpStream` yourself and using `client_async_tls_with_config`. Linux-only `TCP_USER_TIMEOUT` bounds unacknowledged writes [secondary].
4. Write timeouts: a send stuck for longer than WRITE_TIMEOUT means the connection is dead.

## TLS

Prefer rustls. On macOS, corporate MDM roots and proxies live in the Keychain, so use `rustls-tls-native-roots` or a custom `Connector::Rustls(Arc<ClientConfig>)` built with rustls-platform-verifier 0.7.1 (which uses the OS verifier) [secondary]. Pass the connector through `connect_async_tls_with_config`. Build the `ClientConfig` **once** and share it, because loading native roots per reconnect costs CPU. Certificate pinning is optional; if you use it, pin the SPKI of an intermediate or your own CA and ship a backup pin.

## Token refresh

- The token lives in a `watch::Sender<Option<SecretString>>`, owned by an `AuthService` task (single-flight). Consumers `borrow()` it at connect time.
- Refresh at about 80% of the TTL **by wall clock** (`exp` is wall time), and also right after a wake or clock-jump event, before reconnecting.
- If the handshake returns HTTP 401/403 (tungstenite `Error::Http(response)`), request a forced refresh, retry once, then go to `AuthFailed`: report health and keep backoff at its cap. Never hot-loop.
- Send the token in a header (`Authorization`), not in the URL, so it cannot leak into logs or proxies.

## Resume / replay protocol (design)

- Client → `Hello { agent_id, boot_epoch, last_server_seq, outbox_low_water }`. Server → `Welcome { resume: true|false, server_seq }`. If `resume = false`, the server sends a full **snapshot**.
- Client outbox: SQLite table `(seq INTEGER PRIMARY KEY, kind, key, body, created_ms)`. State kinds are **upserted per key** (bounded by the number of keys). Fact kinds are appended. The gateway acks the highest contiguous seq, and the client deletes rows at or below the ack in the writer thread.
- Every message is idempotent: facts carry `seq` or an idempotency key, and state carries a `version`. Duplicate replays are harmless.
- Order: connect, hello, receive snapshot or resume, replay the outbox, then live traffic. Rate-limit the replay (chunks of N per write) so heartbeats keep flowing.

## Sleep/wake, clock changes, network change

- macOS `Instant` does **not** advance during sleep (`CLOCK_UPTIME_RAW`, "does not increment while the system is asleep") [primary, std `sys/time/unix.rs`]. A 20 s ping interval started just before sleep fires about 20 s of **awake** time after wake, and the socket is almost certainly dead by then. So on wake: emit a `ClockJumped`/`DidWake` event, then probe immediately (send a ping and use a short deadline, for example 5 s) or simply reconnect.
- Windows `Instant` (QPC) **does** advance during sleep [secondary: Rippling engineering blog; std docs say the behaviour is unspecified]. Tokio timeouts can therefore expire en masse after resume. Treat this as normal: after wake, everything reconnects.
- Wake sources: the macOS adapter uses IOKit `IORegisterForSystemPower` (`kIOMessageSystemWillSleep` / `kIOMessageSystemHasPoweredOn`) on a CFRunLoop thread [secondary]. Linux uses the logind D-Bus `PrepareForSleep` signal [secondary]. Windows uses `SERVICE_CONTROL_POWEREVENT` / `PBT_APMRESUMEAUTOMATIC` in a service handler [secondary]. The **portable fallback**, which is always on, is the wall-vs-monotonic jump detector (time-config-secrets.md).
- Network change: macOS `NWPathMonitor` (Network.framework) [secondary]. On a path change, reconnect immediately with a small jitter, bypassing the backoff once. Existing sockets bound to the old interface are dead even if they look open.
- Wall-clock changes (NTP step, a user changing the time): durations are monotonic, so they are unaffected. Only token expiry and scheduling read wall time, so re-evaluate both on a jump.

## Uploads (audio, images)

Do not send multi-MB messages on the control socket: one big write head-of-line blocks pings and acks for its whole duration. Prefer an HTTP upload (resumable, with its own timeouts) referenced by id over WS. If you must use WS, use binary chunks of 64 KiB or less, keep a window of N unacked chunks, interleave control frames, and give the transfer an overall deadline.
