# Roadmap — ordered vertical slices

Reordered 2026-11-19 (Sito): fogar-agent becomes a real client that exercises the whole gateway
pipeline before anything that depends on the gateway's coming home/users model. S2 = text and
push-to-talk conversation; robustness moves to S3; login, administration and history wait for the
gateway's users (S4). Previous order: 2026-11-14 (companion app into S1).
Product name: Fogar Desktop (agent + companion app). Specs, decisions and the detailed roadmap
live in the Obsidian vault («Roadmap - fogar-agent en Rust (slices)», «Spec - fogar-agent S2 Hablo
y escribo con Fogar desde el Mac»).

Each slice crosses every ring it needs, ships acceptance + contract + soak tests, and passes a
**measurement gate** (fogar-energy-budget acceptance gate, on the M2 once a macOS adapter is real:
idle CPU <= 1 %, wakeups <= 0.5/s target / 2/s hard, phys_footprint <= 50 MB, no growth slope).

| # | Slice | User-visible outcome | Measurement gate | Status |
|---|---|---|---|---|
| S0 | Walking skeleton | E2E with fakes + mock gateway over a real WS; daemon binary on Linux | Linux only: soak flat in simulated 72 h | Done |
| S1 | Mac connected + companion app | Pairing (`/.well-known`, `POST /pair`) with the key in Keychain; real gateway link (`ws://` on LAN/tailnet, `wss://` via rustls); UI IPC over the Unix socket with peer code-signature check; SwiftUI app in `apps/macos/` styled after the gateway dashboard | macOS CI contract suites for Keychain + IPC; agent and app idle cost on the M2 as the baseline | Done except the energy baseline (#12) |
| **S2** | **Talk and type to Fogar from the Mac** | Conversation window in the app: type and read the reply as it streams; mic button in the composer (hold and release to send, or click to record and then review or send directly; no wake word, no automatic VAD) and the reply plays on the Mac, never without something visible on screen; turn progress (`turn_progress`) and cancel. The gateway's daily conversation, no history list | Turn latencies (first token, first audio) against the gateway's `pipeline_event`; mic-closed idle equal to the S1 baseline, cost while capturing and playing on the M2; xrun/drop counters. **Hard requirement:** single capture-consent choke point | **Spec under review** |
| S3 | Connection that survives everything | Survives gateway restarts, network flaps and sleep/wake with a turn in flight; WS pings + half-open detection | 24 h churn soak on Linux CI + 24 h on the M2: RSS/fd/tasks flat | Pending |
| S4 | Login, administration and history | The agent signs in with the person's account (personal device); admin panel by role; the person's and the home's conversations; agents | Per the gateway | **Waits** for the gateway's users slice |
| S5 | Wake word with our own model | "ok fogar" opens a turn hands-free | False accepts/h, false rejects, duty-cycled CPU on battery | Pending |
| S6 | Mac state to the gateway + optional monitoring module | Gateway sees libre/ocupado/no_molestar (needs a gateway contract change) | Probe < 0.1 % CPU, < 1 wake/s; module off = zero threads | Pending |
| S7 | Screenshots / ScreenCaptureKit | Agent captures the screen on request | TCC re-approval handled; capture cost per frame | Pending |
| S8 | Packaging: SMAppService/.app, signing, notarization | Installable, auto-starting, updatable Fogar Desktop | 72 h soak on the M2 (sleep/wake cycles, battery/AC), idle gate vs baseline | Pending |
| S9 | Windows / Linux adapters | Same features on a second OS via new adapter crates, no core change | Per-OS idle gate (ETW/powercfg, perf/PSI) | Pending |

No local SQLite for now: the conversation history lives in the gateway (S4). It comes back only if
an offline cache is needed.

## Before S2: what the code has today (checked 2026-11-19, `main` at `7d29b68`, gateway 3.0.1)

- `fogar-proto::audio` already matches the gateway 3.0 formats (upload f32le 16 kHz mono; TTS
  `0xA1` + `turn_seq` u16 BE + PCM16 LE 24 kHz mono). `fogar-proto::server` lacks `turn_progress`
  (gateway #254; today it decodes as `Unknown`) and `pipeline_event.elapsed_ms`; the handshake
  does not request `"status"`, which both need.
- `fogar-core::turn` is built for continuous VAD; it has no typed text (`send`), no user cancel, no
  push-to-talk entry, and it drops the reply text.
- `fogar-core::engine` opens the microphone at startup and keeps it open: S2 inverts that (mic
  closed except during an utterance the person started). There is no IPC -> engine channel, and
  `SessionUp` does not carry the `ready` capabilities.
- The audio port has no way to request the OS permission (`mic_permission()` never prompts).
- The macOS audio adapter is a stub (`Unsupported`).
- IPC v3 has nothing for conversation; `StatusReport` does not say whether STT/TTS are live.
- `fogar-agentd` has no embedded `Info.plist` (`NSMicrophoneUsageDescription`) nor the
  `com.apple.security.device.audio-input` entitlement, and in S1 it is started from a shell, so
  TCC would attribute the microphone to the terminal. S2 starts with a TCC spike on the Mac.

## Requirements carried from the slice-0 review (2026-11-13)

**S2 — HARD requirement before any real capture ships (review sec#8; was S3):** a single
capture-consent choke point in fogar-core. Every capture start (mic now, screen in S7) goes through
one function that checks local consent state and is unit-tested; gateway messages can never start
capture by themselves (allowlist, fogar-secure-rust NET-3/CAP). Proposed shape (S2 spec v2,
pending approval):
- The audio path is split in three: **capture** (the `AudioSource` adapter: 16 kHz mono chunks,
  knows nothing about turns or UI), the **utterance** in `fogar-core::capture` (an audio segment
  with a `Start`, an `End` and a `Then` = send / review / discard), and **playback**. UI gestures
  (hold-and-release, click-to-start then stop-to-review or send-now) and, later, VAD and wake word
  are only sources of `Start`/`End`; capture and what happens after the end do not change.
- `CaptureGate` is the only owner of the `AudioSource` and the only caller of `open`. In S2 the
  only `Start` is `UserAct`, which only the core IPC module can build from a same-signer,
  conversation-subscribed `capture_start`; it also requires OS permission `Granted`, a ready link
  with a live transcriber and no other open utterance. It closes on the user's `End`, on that IPC
  connection closing (discard), on a privacy backstop for click-started utterances, on link loss
  and on shutdown. The wake-word `Start` (S5) will need its own explicit opt-in.
- Consent is core logic, not a port: what comes from outside already has ports (OS permission in
  `AudioSource`, the user's act over IPC). Enforced by an archtest rule and by a property test (no
  gateway message sequence ever opens the fake mic). The contract PR carries the audio-port change
  (permission request), the fakes and a consent suite in `fogar-testkit`.

**S2 — product requirement (Sito, 2026-11-19): Fogar may speak at any time, also with the app
closed, but never without something visible on screen.** `PlaybackGate` is the only writer to the
`AudioSink`; a turn's audio plays only after the app confirms it is showing that turn
(`turn_visible`): a floating notice (non-activating, above full-screen apps) with the live text of
what Fogar says and a Stop button. If no app is connected, the agent opens it hidden through a new
`UiLauncher` port (`open -g -j -b com.fogar.desktop` on macOS; GUESSED from a LaunchAgent, checked
in the TCC spike). Audio waiting for confirmation is bounded (8 s) and dropped after 5 s without it;
the turn then stays as text. Word-by-word text/voice sync comes later.

**Gateway constraint found in the S2 review:** the gateway's silence watchdog closes the session
(1000, `silence_timeout`) after ~6–8 s of audio without any transcription, with the per-client
defaults (`silence_timeout_s` 2, `max_silence_turns` 3). Rule (Sito): an automatic cut never sends.
In S2 the agent ends the utterance itself after 5 s without a new partial (or at the gateway's
second silence notice) with `Then = Review`: the transcription goes to the composer, unsent. Same
for a 5-minute privacy backstop. With a client VAD (S5) the cut uses the VAD and still never sends;
only the wake-word mode sends on its own, after a short pause. A gateway issue (don't close the
session during an explicit utterance) is proposed, not blocking.

**S1 gates (done):** `cargo vet` and `cargo geiger` in `scripts/gates.sh` + CI (#15); fuzz targets
for `decode_server`, `parse_tts_frame` and the IPC framing (#13); the agent-team `ROOT_HASH`
acknowledgement job (#11); `wss://` via rustls (#5).
