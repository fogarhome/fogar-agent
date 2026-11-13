# Roadmap — ordered vertical slices

Each slice crosses every ring it needs, ships acceptance + contract + soak tests, and passes a
**measurement gate** (fogar-energy-budget acceptance gate, on the M2 once a macOS adapter is real:
idle CPU <= 1 %, wakeups <= 0.5/s target / 2/s hard, phys_footprint <= 50 MB, no growth slope).

| # | Slice | User-visible outcome | Measurement gate |
|---|---|---|---|
| S0 | Walking skeleton (this) | E2E with fakes + mock gateway over a real WS; daemon binary on Linux | Linux only: soak flat in simulated 72 h; no M2 numbers |
| S1 | Real WS client robustness + SQLite sessions | Survives gateway restarts, network flaps, sleep/wake; wss via rustls; WS pings + half-open detection; pairing (`/.well-known`, `POST /pair`) storing the key in Keychain; conversation/session log in SQLite (one writer thread, WAL) | 24 h real-time churn soak on Linux CI + 24 h on the M2: RSS/fd/tasks flat, WAL bounded, idle wakeups from timers declared |
| S2 | Real mic capture + VAD on the Mac | Speaking to the Mac opens a turn; replies play on the speaker | Mic-open idle CPU/wakeups on the M2, xrun/drop counters, TCC prompt attribution, 72 h RSS |
| S3 | Wake word with our own model | "Hey Fogar" opens a turn hands-free | False accepts/h, false rejects, duty-cycled CPU on battery |
| S4 | UI IPC over Unix socket + Swift app skeleton | Menu-bar app shows status, toggles listening | Peer code-signature check verified; IPC idle cost 0 wakeups |
| S5 | Optional system-probe / monitoring module + load state to gateway | Gateway sees libre/ocupado/no_molestar (needs a gateway contract change) | Probe < 0.1 % CPU, < 1 wake/s; module off = zero threads |
| S6 | Screenshots / ScreenCaptureKit | Agent can capture the screen on request | TCC re-approval handled; capture cost per frame |
| S7 | Packaging: SMAppService/.app, signing, notarization, 72 h soak on a Mac | Installable, auto-starting, updatable agent | 72 h soak on the M2 (sleep/wake cycles, battery/AC), idle gate vs baseline |
| S8 | Windows / Linux adapters | Same features on a second OS via new adapter crates, no core change | Per-OS idle gate (ETW/powercfg, perf/PSI) |

## Requirements carried from the slice-0 review (2026-11-13)

**S1 — gates to add (fogar-secure-rust §9), not optional:**
- `cargo vet` (audits/exemptions committed) and `cargo geiger` (unsafe count reported per PR,
  must not grow silently) in `scripts/gates.sh` + CI.
- Fuzz targets (cargo-fuzz) for `fogar_proto::decode_server`, `parse_tts_frame` and the IPC
  framing; built on every PR, run on a schedule.
- CI compares the agent-team `ROOT_HASH` (ADR-0007) against the main-branch value and requires an
  explicit acknowledgement when it changes.
- TLS (`wss://`, rustls) replaces the loopback `ws://` warning (ADR-0006 item 4); a secret-aware
  frame type so the handshake carrying the client key can be zeroized.

**S2 — HARD requirement before any real capture ships (review sec#8):** a single capture-consent
choke point in fogar-core. Every capture start (mic now, screen in S6) goes through one function
that checks local consent state (user toggle / TCC state / "listening" indicator) and is
unit-tested; gateway messages can never start capture by themselves (allowlist, fogar-secure-rust
NET-3/CAP). The S2 contract PR must include the port + fake + contract suite for it.
