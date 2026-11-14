# TCC (privacy permissions) for a headless agent

**This is the area with the least documentation and the highest risk. Treat every behaviour here as must-test-on-hardware.** Quinn: "there is basically no documentation for the TCC edge cases that crop up on macOS" [secondary: forum 853725].

## 1. Concepts
- TCC checks the **responsible code**, not simply the calling process. "When a process performs a privileged operation, macOS has to track down the responsible code" [secondary: forum 805245]. "it's common for there to be responsible code but no responsible process… if an app has a login item, the login item might be running when the app is not" [secondary: forum 731504]. There is no public API to set responsibility [secondary].
- A child spawned by an app usually inherits responsibility (chain-walking). DTS confirmed this for Input Monitoring on macOS 15.7.1, but "There are also situations where macOS doesn't follow the chain as you might expect" [secondary: 805245].
- A launchd-launched agent is **not** a child of the UI app, so no chain-walk to the UI app happens. Quinn: "TCC's ability to trace from a daemon to its container app is rather limited, even if the daemon is installed via SMAppService" [secondary: 853725]. For a bare tool run by a LaunchAgent, "TCC subsystem has to be able to identify some app as the responsible code for your agent" [secondary: 694948].
- Per-user vs system: a root LaunchDaemon cannot inherit per-user grants [secondary]. This is one more reason fogar-agent is a **LaunchAgent**.

## 2. Decision: give the agent its own stable TCC identity
**Ship the agent as an app-like bundle `FogarAgent.app`** (own `CFBundleIdentifier` `com.fogar.agent`, `CFBundleName` "Fogar Agent", `LSUIElement`=YES, usage strings, hardened-runtime entitlements), nested in `Fogar.app`, and launched by SMAppService via `BundleProgram`.

| Option | Pro | Con |
|---|---|---|
| **A. App-like helper bundle (recommended)** | Clear name in prompts, System Settings and Activity Monitor. Grants keyed to a stable DR. Works when the UI app is not running. Can carry a provisioning profile if a restricted entitlement is ever needed | User grants permissions to "Fogar Agent", a second name. Quinn confirms the trade-off: "makes it harder for the user to enable Accessibility" [secondary] |
| B. Bare binary in `Contents/MacOS` next to the UI app | One forum user found Accessibility "inherited" this way. Quinn: "I'm surprised that works" [secondary] | Undocumented, may break in any release, and both processes share a name |
| C. UI app owns all TCC-gated capture and streams to the agent | Single identity | Capture stops whenever the UI quits, which defeats an always-on agent |

Bare binary without an Info.plist: no purpose string. Accessing protected resources without a purpose string crashes the process (`__TCC_CRASHING_DUE_TO_PRIVACY_VIOLATION__`) [secondary: forum 728814 title + common knowledge, unverified for this case].

## 3. Per permission

| Permission | Check API (Rust, verified to exist in objc2 0.3.2 crates) | Request behaviour | Notes |
|---|---|---|---|
| Microphone | `AVCaptureDevice::authorizationStatusForMediaType` / `requestAccessForMediaType_completionHandler` (objc2-av-foundation) [primary: bindings exist] | Prompt shows the responsible app name. Needs the `com.apple.security.device.audio-input` entitlement (hardened runtime) **and** `NSMicrophoneUsageDescription` | Whether a prompt can appear for an LSUIElement agent launched by launchd is [unverified] |
| Screen recording (ScreenCaptureKit) | `CGPreflightScreenCaptureAccess()` / `CGRequestScreenCaptureAccess()` (objc2-core-graphics) [primary: bindings exist] | Request usually sends the user to System Settings, and a **restart of the process** may be required after granting [unverified] | Since macOS 15 Sequoia, a recurring re-approval prompt appears (weekly in beta, **monthly** at release, fewer for regularly used apps in 15.1) unless the app uses `SCContentSharingPicker`, MDM sets `forceBypassScreenCaptureAlert`, or the app holds the restricted persistent-content-capture entitlement (VNC-type apps) [secondary: mjtsai/9to5mac 2024]. **Design for it:** detect revoked/expired access and surface it in the UI. Status on macOS 26 is [unverified] |
| Accessibility | `AXIsProcessTrusted()` / `AXIsProcessTrustedWithOptions(options)` with the prompt option (objc2-application-services) [primary: bindings exist] | Opens a prompt to System Settings › Privacy & Security › Accessibility | Grants are keyed to the code signature. Re-signing with a different identity or identifier drops the grant [unverified] |
| Input Monitoring (if keyboard events are needed) | IOHID access check APIs [not researched] | | |

## 4. Rules
1. **Never request permissions at daemon startup.** The UI drives it: the UI asks the agent over IPC to call the request API, so the prompt is attributed to FogarAgent at a moment the user expects. The agent reports `PermissionState` events.
2. Model each permission as `NotDetermined | Denied | Granted | Restricted | Unknown`, plus `last_checked`. Re-check on wake, on UI connect, and before starting a capture. **Do not poll on a timer.**
3. Keep signing identity, identifier and Team ID stable, or users lose their grants after an update.
4. During development, `tccutil reset <Service> com.fogar.agent` resets state (services e.g. `Microphone`, `ScreenCapture`, `Accessibility`) [secondary: Apple doc "Resetting access to protected resources in macOS", not fetched in full].
5. A ScreenCaptureKit stream must be stopped (`stopCapture`). Leaked streams were reported to make later `SCShareableContent` calls hang [secondary/weak: StackOverflow answer]. Put a timeout on every SCK async call.

## 5. Must verify on a real Mac (Sonoma 14, Sequoia 15, Tahoe 26)
- Does the microphone prompt appear, attributed to "Fogar Agent", when the agent is launched by launchd from SMAppService?
- Same for screen recording: is the grant listed under Fogar Agent or under Fogar? Is a process restart needed after granting?
- Do grants survive a Sparkle update (same DR)? Do they survive the user moving the app?
- Does `LimitLoadToSessionType=Aqua` matter, and what happens at the loginwindow / with fast user switching?
- Does ScreenCaptureKit work in a process that never creates an `NSApplication`? If not, the agent must run `NSApplication` (accessory policy) on its main thread. Track this as a top risk.

## 6. Windows / Linux
- Windows: per-app privacy toggles for microphone (desktop apps are listed under "Let desktop apps access your microphone"). There is no prompt for screen capture via Windows.Graphics.Capture/DXGI duplication in desktop apps [unverified]. Cost: low.
- Linux (Wayland): screen capture only through `xdg-desktop-portal` ScreenCast with a user dialog and restore tokens. Microphone through PipeWire, usually without a prompt. Cost: medium (portal flow + restore tokens). X11: no permission model.
- The core `Permissions` port must allow "no concept" (`Unsupported`) and "needs user dialog" states.
