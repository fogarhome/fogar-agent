# Microphone permission (TCC) for a headless LaunchAgent — macOS, all UNTESTED

Packaging, signing, SMAppService and launchd keys are owned by the sibling skill
`fogar-macos-daemon` (see its `references/tcc-permissions.md`, `launchd-smappservice.md`). This file
covers what the **audio** code must do and assume.

## What Apple documents [primary]
- "In iOS and macOS 10.14 and later, the user must explicitly grant permission for each app to access
  the camera and microphone." The system remembers the answer; users change it in Settings ▸ Privacy.
- Include `NSMicrophoneUsageDescription` in the app's `Info.plist`.
- macOS: enable the entitlement `com.apple.security.device.audio-input`.
- "Your app needs to contain the appropriate key in its Info.plist file, and the appropriate
  entitlement enabled in macOS, before it requests authorization or attempts to use a capture device.
  **Otherwise, the system terminates your app.**"
  (Source: AVFoundation article "Requesting authorization to capture and save media".)
- Hardened Runtime (required for notarization) lists `com.apple.security.device.audio-input` under
  *Resource Access*. Apple DTS (2026, macOS 26.5.1 thread): notarized app got "User rejected
  permission" with no prompt → cause: missing `audio-input` entitlement under the hardened runtime;
  verify with `codesign -d --entitlements - /path/to/App.app` [secondary: Apple forums 835256, DTS].
- `AVCaptureDevice authorizationStatusForMediaType:` / `requestAccessForMediaType:completionHandler:`
  (objc2-av-foundation 0.3.2: `AVCaptureDevice::authorizationStatusForMediaType(&AVMediaType)`,
  `requestAccessForMediaType_completionHandler`, `AVMediaTypeAudio: Option<&'static AVMediaType>`).
  Header text: "This call will not block… **Until access has been granted, any AVCaptureDevices for
  the media type will vend silent audio samples** … The user is only asked for permission the first
  time" [primary: header doc copied into objc2-av-foundation].
- Menu bar: "An orange dot … next to the Control Centre icon … indicates the microphone on your Mac is
  in use" [primary: Apple Support, Control Centre].

## What Apple does NOT document (DTS statements) [secondary: Apple Developer Forums]
- TCC attributes requests to the **responsible code**; helpers spawned by an app inherit it (thread
  701094: fix was to put the usage keys in the *main app's* Info.plist).
- "there is basically no documentation for the TCC edge cases that crop up on macOS (multiple instances
  of an app, user vs system-wide privileges, launchd daemons and agents, and so on)" — Quinn, thread
  853725 (2026).
- A LaunchDaemon cannot inherit its container app's per-user TCC grants; "TCC's ability to trace from a
  daemon to its container app is rather limited, even if the daemon is installed via SMAppService".
  → Microphone capture must run in a **per-user LaunchAgent** inside the user session.

## Recommended shape (to verify on the M2)
1. Agent executable lives in an app-like bundle (`FogarAgent.app/Contents/MacOS/fogar-agent`) with its
   own `Info.plist`: `CFBundleIdentifier`, `LSUIElement=true`, `NSMicrophoneUsageDescription`.
   Signed (Developer ID) with hardened runtime + `com.apple.security.device.audio-input`.
2. Registered with `SMAppService.agentServiceWithPlistName:` (objc2-service-management 0.3.2 exposes
   `agentServiceWithPlistName`, `registerAndReturnError`, `status`) from the UI app; the plist lives in
   the app's `Contents/Library/LaunchAgents` and may use `BundleProgram` [primary: header text].
3. First run: query `authorizationStatusForMediaType(AVMediaTypeAudio)`:
   - `NotDetermined` → do **not** silently start capture from launchd at login; surface "needs
     permission" to the UI (IPC) and let a user-visible action trigger `requestAccessForMediaType…`.
     Whether a prompt can appear when the request comes from the launchd-started `LSUIElement` agent is
     [unverified] — test both "agent requests" and "UI app requests" and check which identity appears in
     System Settings ▸ Privacy ▸ Microphone.
   - `Denied`/`Restricted` → don't open the stream; report state; poll again on user action.
   - `Authorized` → open.
4. A bare Mach-O with an embedded plist (`-sectcreate __TEXT __info_plist`) is a known trick for CLI
   tools [secondary]; not recommended for a shipped agent (unstable TCC identity across updates [unverified]).
5. Dev reset: `tccutil reset Microphone <bundle-id>` [secondary].

## What the audio code must assume
- **Permission can be revoked at runtime** in System Settings. Expected behaviour for a running
  CoreAudio/AUHAL input when revoked or never granted: silence (zeros) rather than an error
  [unverified for AUHAL; documented for AVCaptureDevice]. cpal has `ErrorKind::PermissionDenied`, but
  no CoreAudio code path in 0.18.2 was seen raising it during streaming [primary: source grep].
  → core supervisor: "digital silence detector" = all samples exactly 0.0 for > 5 s (real mics always
  have noise) → re-query `mic_permission()`, report `PossiblyNoPermission`, back off.
- Keep `mic_permission()` **non-prompting**; only an explicit user flow may call `requestAccess…`.
- The completion handler of `requestAccess…` runs on an arbitrary dispatch queue — forward the result
  over a channel; don't touch audio state from it.

## Power-related system events (adapter of a `SystemEvents` port)
- `IORegisterForSystemPower` + CFRunLoop thread; must `IOAllowPowerChange` on
  `kIOMessageSystemWillSleep`/`kIOMessageCanSystemSleep` (else sleep is delayed up to 30 s); do not
  acknowledge `kIOMessageSystemHasPoweredOn` ("1-5 or more seconds after initiating system wakeup")
  [primary: IOKit header text via objc2-io-kit].
- Audio policy: on WillSleep → stop capture (drop handle), flush/close any uplink; on HasPoweredOn →
  wait ~1–2 s, re-query default device + permission, reopen. The heartbeat watchdog remains the
  safety net if notifications are missed.

## launchd `ProcessType` vs audio
- Apple: "Always include the `ProcessType` key in your daemon or launch agent's Info.plist… The system
  uses that key to determine your daemon's purpose and adjust its available resources" [primary: Apple
  Silicon tuning guide]. `Adaptive` relies on XPC context [primary] — useless with sockets.
- Sibling skill defaults to `Background`. Risk for audio: resource limits on the dsp thread → ring
  overflow. The CoreAudio IO thread itself belongs to the HAL client machinery in our process; whether
  `Background` affects it is [unverified]. Measure `frames_dropped`, wake latency and energy with
  `Background` vs `Standard` (unset) during a 24 h soak.
