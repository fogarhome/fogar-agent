---
name: fogar-macos-daemon
description: >-
  How to build, package and run fogar-agent, the always-on headless Rust daemon, on macOS
  (fanless MacBook Air M2 first; Windows/Linux later), and keep it portable through hexagonal ports.
  Use when touching: launchd / LaunchAgent plists, SMAppService.agent registration from the
  SwiftUI app, app bundle layout (Contents/Library/LaunchAgents, BundleProgram), KeepAlive /
  RunAtLoad / ProcessType / LowPriorityIO / Nice / ThrottleInterval, crash loops, restarts;
  IPC between the Swift UI and the daemon (Unix domain socket, LOCAL_PEERTOKEN, audit token,
  SecCode code-signing requirement, XPC / MachServices / NSXPCConnection, named pipes, systemd
  socket paths); codesign, hardened runtime, entitlements, notarytool, stapler, Developer ID,
  Sparkle updates; TCC permissions (microphone, ScreenCaptureKit screen recording, accessibility,
  responsible process) for a headless helper; os_log / unified logging via tracing; crash
  reports; sleep/wake, display, power source, Low Power Mode, thermal events; objc2, block2,
  dispatch2, objc2-io-kit, objc2-security, run loops with tokio; Swift interop (typeshare vs
  UniFFI); daemon self-update and version handshake; Windows service / Task Scheduler and
  systemd user unit equivalents.
---

# fogar-agent on macOS: daemon shell skill

Scope: the **process shell** (lifecycle, packaging, IPC, OS events, permissions, logging, update). Domain features live elsewhere.
Status: researched 2026-11-12 from primary sources on Linux. **Nothing has been run on a Mac.** Facts are tagged in `references/` and `RESEARCH.md`. The tags [primary]/[secondary]/[unverified] matter: treat [unverified] as a hypothesis with a test attached.

## 1. Decisions (default answers; deviate only with a measured reason)

| Topic | Decision | Why (details in references/) |
|---|---|---|
| Service type | **LaunchAgent registered by the UI app with `SMAppService.agent(plistName:)`**, plist in `Fogar.app/Contents/Library/LaunchAgents/com.fogar.agent.plist`, `BundleProgram` relative path | Per-user GUI session (needed for TCC), shown in Login Items, no installer scripts. A LaunchDaemon (root) cannot use per-user TCC grants |
| Agent packaging | **App-like helper `FogarAgent.app`** (own bundle id `com.fogar.agent`, `LSUIElement`, usage strings) nested in `Fogar.app` | Gives TCC a stable responsible identity independent of the UI. Allows provisioning profiles later. Exact nested location must be verified (§6) |
| plist keys | `RunAtLoad=true`, `KeepAlive={SuccessfulExit=false}`, `ThrottleInterval=30`, `ProcessType=Background`, `LowPriorityIO=true`, `Nice=5`, `ExitTimeOut=10`, `LimitLoadToSessionType=Aqua` | launchd.plist(5). Measure Background vs Standard for UI latency |
| IPC | **Unix domain socket** in `~/Library/Application Support/com.fogar.agent/run/agent.sock` (dir 0700). **Mutual code-signature check** via `getsockopt(SOL_LOCAL, LOCAL_PEERTOKEN)` → `SecCode::copy_guest_with_attributes` (kSecGuestAttributeAudit) → `check_validity` against a Team-ID designated requirement. Length-prefixed JSON frames + mandatory `Hello` version handshake | Portable to Linux UDS / Windows named pipes. Rust XPC has no maintained crate (hand FFI to libxpc = moderate pain). XPC stays a possible later adapter behind `IpcTransport` |
| Swift types | **typeshare-generated `Codable`** from `fogar-protocol`. Hand-written Swift framing. UniFFI only if the Swift client grows past ~500 lines | Separate processes: share the contract, not the code |
| Logging | `tracing` in core. macOS binary installs `tracing-oslog` (subsystem `com.fogar.agent`) | It logs everything `%{public}s` and makes an os_activity per span, so no user data in fields and no spans in hot paths |
| Update | **Sparkle replaces the whole app.** New UI handshakes; an older agent exits with code 75 → launchd relaunches the new binary. Never self-replace the binary | DTS: the system does not relaunch helpers after an update. "exit() and let launchd open your new version" |
| Threads | Main thread = Apple run loop (CFRunLoop/NSRunLoop; NSApplication only if ScreenCaptureKit requires it). tokio on a background thread (1–2 workers). Apple callbacks on a private `dispatch2` serial queue → `mpsc` into tokio | objc2: frameworks "will block indefinitely or fail" without a running main run loop. Do not use `#[tokio::main]` in the mac binary |

## 2. Hard rules
1. The daemon **never forks/daemonizes**, stays in the foreground, handles **SIGTERM** (flush, close socket, exit 0) within `ExitTimeOut`.
2. **Freeze identity:** Team ID, signing identifier `com.fogar.agent`, bundle id, plist label and socket path never change across versions. An identifier change caused AMFI launch-constraint failures on upgrade, and it resets TCC grants.
3. **Sign inside-out, never `--deep`**: `codesign -s "Developer ID Application: …" -f --timestamp -o runtime [--entitlements] <helper .app>`, then Sparkle's nested code, then `Fogar.app`. Strip before signing.
4. Agent entitlements: only what is used: `com.apple.security.device.audio-input` for microphone. Screen recording and Accessibility need no entitlement. Never `get-task-allow`, `disable-library-validation`, `allow-dyld-environment-variables`.
5. Notarize with `notarytool submit --wait`, read the log, `stapler staple` the **.app or .dmg** (zips cannot be stapled).
6. **All `unsafe`/FFI lives in `fogar-adapter-macos`.** Core gets owned, `Send` Rust types only. Wrap Objective-C call batches made off GCD in `objc2::rc::autoreleasepool`. `catch_unwind` in every C callback.
7. **Zero idle wakeups:** no polling timers when idle. Event-driven everything. CPU <1% and footprint <50 MB are **targets to measure** (`top -stats pid,cpu,idlew,power`, `footprint`), not facts.
8. Power: on `kIOMessageSystemWillSleep` pause, flush, then **`IOAllowPowerChange`** (the adapter force-acks after 5 s; the OS waits up to 30 s). On `kIOMessageCanSystemSleep` always allow; **never veto sleep**. Never ack `WillPowerOn`/`HasPoweredOn`.
9. Permissions are requested **only on user action from the UI** (the agent performs the request so the prompt is attributed to it). Re-check on wake / UI connect / before capture, never on a timer. Handle Sequoia's periodic screen-recording re-approval as a normal state.
10. **No user content in logs.** Panic hook → os_log (public, content-free) then abort (`panic = "abort"`). Keep dSYMs.
11. Crash-loop breaker in core: ≥5 starts in 10 min → **safe mode** (stay alive, IPC + diagnostics only). launchd itself only throttles and never gives up.
12. Socket: refuse to start if the path is ≥104 bytes (`sun_path[104]`). Single instance via `flock`. Unlink a stale socket only after `connect` fails. Never trust PID (`LOCAL_PEERPID`, tokio `peer_cred`) for auth.
13. Load all bundle resources at startup. After a Sparkle swap the old bundle path is invalid.
14. SMAppService calls happen in the Swift app, **off the main thread** (latency unknown). There is no status-change notification: poll `status` on app activation. `NotRegistered=0, Enabled=1, RequiresApproval=2, NotFound=3`. For `requiresApproval` → `openSystemSettingsLoginItems()`. Unregister → register needs retry/backoff (observed "Operation not permitted" race).

## 3. Ports (traits in `fogar-core`; adapters per OS)
`ServiceHost` (shutdown signal, ready, watchdog, `ExitIntent{Stop|RestartForUpdate|Fatal}`) · `IpcTransport` (bind/accept/connect → `PeerIdentity`) · `SystemEvents` (SleepImminent{ack}, Woke, Display, PowerSource, LowPower, Thermal, SessionActive) · `Permissions` (Microphone/ScreenCapture/Accessibility; states incl. `Unsupported`, `NeedsUserDialog`) · `Paths`. `ServiceRegistrar` is **outside** the daemon (Swift UI / installer).
Core owns: supervisor, auth policy, crash-loop/safe mode, update handshake, migrations, power-aware scheduling. Core must build and test on Linux with fakes. CI forbids `objc2*`/`windows*`/`zbus` in core. See `references/ports-and-portability.md`.

## 4. Windows / Linux equivalents (summary)
| macOS | Linux | Windows |
|---|---|---|
| LaunchAgent via SMAppService | systemd **user** unit, `Restart=on-failure`, `RestartSec`/`RestartSteps`, `StartLimitBurst`, `Nice=`, `IOSchedulingClass=idle`, `CPUWeight=`, `MemoryHigh=`, `Type=notify`+`WatchdogSec` | Per-user **logon task** (Task Scheduler) or HKCU Run. **Not a service**: services run in session 0 without the user's desktop/audio |
| UDS + audit token + SecCode | UDS in `$XDG_RUNTIME_DIR` + `SO_PEERCRED` | Named pipe + user-SID DACL + `first_pipe_instance(true)` + `reject_remote_clients(true)` |
| IOKit power, NSProcessInfo | logind `PrepareForSleep` + delay inhibitor, UPower | `WM_POWERBROADCAST`, power setting notifications |
| TCC | xdg-desktop-portal (Wayland screencast), PipeWire | Privacy toggles |
| os_log | journald | file/ETW |
| Developer ID + notarization + Sparkle | package signing / repo | Authenticode + installer/MSIX |
Porting cost: Linux low–medium (Wayland capture is the expensive part), Windows medium (pipes DACL, update file locking, power window).

## 5. Pitfalls (each one has bitten someone)
- `#[tokio::main]` on the main thread, then Apple APIs waiting forever for a run loop.
- `tracing` spans per audio buffer → one os_activity each → CPU/energy burn.
- Bare Rust binary touching the mic without an Info.plist purpose string → TCC kills it.
- Running the agent from `target/debug` via a hand-made plist and expecting TCC prompts or grants to carry over to the signed build. They are keyed to the code signature.
- `ProcessType=Adaptive` with sockets: it only reacts to XPC transactions.
- `StandardOutPath` log files growing for days.
- Expecting launchd to give up on a crash loop. It restarts every `ThrottleInterval` forever.
- Re-registering immediately after `unregister` → transient failure. Use the completion handler + backoff.
- Changing the plist between versions without unregister/register → launchd keeps the old config [secondary].

## 6. Verify on a real Mac before relying on it (blocking checklist)
- [ ] `register()` accepts the agent at `Contents/Helpers/FogarAgent.app/...` via `BundleProgram` (else use `Contents/Library/LoginItems/` or `Contents/MacOS/`).
- [ ] Microphone and screen-recording prompts appear and are attributed to "Fogar Agent" when launchd starts it, and grants survive a Sparkle update and an app move.
- [ ] ScreenCaptureKit works without `NSApplication`. If not, run an accessory `NSApplication` on the main thread.
- [ ] Whether `KeepAlive{SuccessfulExit=false}` restarts after SIGABRT (panic) and after exit 75. Measure the actual throttle.
- [ ] `LOCAL_PEERTOKEN` on the client side yields the server's token. The SecCode check passes for Developer ID and dev builds with the intended requirement strings.
- [ ] Idle CPU, idle wakeups and footprint for 72 h on battery, lid closed/open, with sleep/wake cycles. Compare `ProcessType` Background vs Standard.
- [ ] `IONotificationPortSetDispatchQueue` delivery of sleep/wake without a main run loop.
- [ ] Latency/blocking of `SMAppService.status`/`register` on macOS 14/15/26.
- [ ] macOS 26 (Tahoe) changes to SMAppService, BTM and the screen-recording re-prompt cadence.

## References
- `references/launchd-smappservice.md`: plist template, key semantics, registration flow, crash loops, debug commands, systemd/Windows mapping
- `references/ipc.md`: UDS vs XPC table, socket path rules, peer verification sketch, wire protocol, named pipes
- `references/signing-notarization.md`: entitlements, signing order, notarytool/stapler, Sparkle, CI
- `references/tcc-permissions.md`: responsible code, per-permission APIs, Sequoia re-prompts, test plan
- `references/apple-apis-from-rust.md`: objc2/dispatch2/block2 threading, power/display events, os_log, crash reports, energy
- `references/swift-interop-and-update.md`: typeshare vs UniFFI, Sparkle update + version handshake
- `references/ports-and-portability.md`: crate layout, port table, porting cost
- `RESEARCH.md`: sources, per-claim confidence, conflicts, open questions
