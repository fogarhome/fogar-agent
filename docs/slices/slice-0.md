# Slice 0 — walking skeleton

**Outcome:** every layer crossed end to end with fakes on Linux: fake mic -> energy VAD -> core
turn machine -> real WebSocket -> protocol-v1 mock gateway -> `0xA1` TTS frames -> fake player;
plus the daemon binary connecting, serving status over a Unix socket and stopping cleanly on
SIGTERM. **Not** an outcome on a real Mac (macOS adapters are untested stubs).

Rings: proto[x] ports[x] core[x] testkit[x] host[x] agent-linux[x] agent-macos[stub] agentd[x] ipc[x (Linux)] ui[ ]

## Contract (ports in `crates/fogar-ports`, fakes + suites in `crates/fogar-testkit`)
| Port | Sync/async | Fake | Contract suite | Real adapter in S0 |
|---|---|---|---|---|
| `Clock` (+ `SuspendBehavior`) | async | `TestClock` (tokio paused) | `clock_contract!` | `fogar-host::TokioClock` |
| `Random` | sync, dyn | `SeededRandom` | `random_contract!` | `fogar-host::PrngRandom` |
| `GatewayLink`/`LinkSession` | async | `FakeGatewayLink` (in-memory v1 gateway) | `gateway_link_contract!` | `fogar-host::WsGatewayLink` |
| `AudioSource` | sync, dyn | `FakeMic` | `audio_source_contract!` | Linux/macOS: explicit Unsupported |
| `AudioSink` | sync, dyn | `FakeSpeaker` | `audio_sink_contract!` | Linux/macOS: explicit Unsupported |
| `SecretStore` | sync, dyn | `MemorySecretStore` | `secret_store_contract!` | Linux 0600 file; macOS stub |
| `SystemProbe` | sync, dyn | `FakeSystemProbe` | `system_probe_contract!` | Linux /proc; macOS getloadavg (untested) |
| `PowerPolicy` | sync, dyn | `FakePowerPolicy` | `power_policy_contract!` | Linux Unsupported; macOS QoS (untested) |
| `IpcServer`/`IpcConn` | async | `FakeIpc` | `ipc_server_contract!` | Linux UDS + SO_PEERCRED; macOS stub |

Wire: `fogar-proto` = protocol v1 (ADR-0005), vectors from PROTOCOL-SPEC.md + proptests.

## Done criteria (all must hold; evidence in `features.json`)
1. `scripts/gates.sh` reports no FAIL (NOT RUN gates listed honestly).
2. E2E over a real WebSocket (`crates/fogar-agentd/tests/slice0_e2e.rs`): handshake without
   `type`, status-before-ready tolerated, `end` -> transcription -> `send`, `turn_start`/`token`/
   `turn_end`, PCM16 bit-exact at the player; barge-in; close classification; keepalive vs idle
   close; gateway down then up.
3. Paused-time acceptance (`crates/fogar-core/tests/`): every spec §11 close row + 1006; backoff
   bounds incl. 500-failure ceiling; keepalive (30/h) prevents the 300 s idle close; not-paired and
   locked-keychain paths; backpressure policies; graceful shutdown back to the live-task baseline.
4. 72 simulated hours with hourly drops: tasks/queues flat, heap growth 0 B after warm-up, and the
   leak canary detects a planted leak.
5. Binary smoke: `fogar-agentd` connects, answers `status` over the Unix socket, SIGTERM -> exit 0
   in < 5 s with a close frame, ledger `start, clean`, key never in stderr.
6. Every fake and every Linux/host adapter passes its contract suite.

## Out of scope (see ROADMAP)
TLS/wss, WS pings + half-open detection, pairing, persistence, real audio, wake word, macOS
packaging/TCC, UI, measurement on the M2.
