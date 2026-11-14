# macOS: code signing, hardened runtime, entitlements, TCC, launchd

Labels: [primary] checked 2026-11-13; [secondary] Apple docs / widely documented, not re-checked;
[unverified]. Re-verify Apple behaviour on each macOS major release.

## Signing and release checks (MAC-1)

- Developer ID Application certificate; hardened runtime (`codesign --options runtime`);
  secure timestamp; notarized with `xcrun notarytool submit ... --wait` and stapled [secondary].
- Verify artifacts in CI: `codesign --verify --deep --strict --verbose=2 <app>`,
  `codesign -d --entitlements - <binary>` (diff against a committed allowlist),
  `spctl --assess --type execute -vv <app>` [secondary].
- Entitlements that must be absent in release unless an approved exception exists [secondary]:
  - `com.apple.security.get-task-allow` (lets debuggers attach / task port access)
  - `com.apple.security.cs.disable-library-validation` (allows loading non-team dylibs)
  - `com.apple.security.cs.allow-dyld-environment-variables` (re-enables `DYLD_INSERT_LIBRARIES`)
  - `com.apple.security.cs.allow-unsigned-executable-memory`, `...allow-jit`
- Hardened runtime needs `com.apple.security.device.audio-input` for microphone access [secondary].
- Every helper binary/dylib inside the bundle is signed with the same Team ID; no unsigned plugins.

## TCC (microphone, screen recording) (MAC-2)

- Microphone requires `NSMicrophoneUsageDescription` in Info.plist and user consent [secondary].
- Screen capture (ScreenCaptureKit / CGWindow APIs) requires Screen Recording permission; recent
  macOS versions re-prompt periodically for screen-recording apps [unverified: exact cadence].
- TCC attributes permission to the **responsible process**; children launched by the agent may act
  under the agent's grants [secondary]. Abuse surfaces to review:
  - spawning shells/SSH/helpers that could capture with our grants;
  - loading code from writable locations (plugins, scripts, `DYLD_*` — blocked by hardened runtime
    unless entitlements above re-enable it);
  - an IPC method that lets another local process trigger capture through us (confused deputy:
    see ipc-peer-auth.md);
  - Apple Events / Accessibility automation if ever requested — avoid unless essential.
- Capture must stop on permission revocation, on screen lock / fast-user-switch [unverified: which
  notifications to observe], and when local policy says off, regardless of gateway commands.
- Visible indicator in UI whenever capture is active; macOS also shows its own mic indicator [secondary].

## launchd and filesystem (MAC-3)

- LaunchAgent (per-user) unless root is truly needed; no LaunchDaemon running as root for a
  per-user capture agent.
- Plist and binary paths not writable by other users; `ProgramArguments` absolute paths into the
  signed bundle; no `EnvironmentVariables` that affect loading.
- Data dirs under the user's Library with `0700`; caches of captured data encrypted or not kept.

## Updates

Updater verifies: code signature valid, Team ID equals ours, designated requirement matches,
version strictly greater (anti-rollback), download over TLS, staged in a private dir, atomic swap.
Never execute an installer script fetched at runtime.

## Review questions

- Did this PR change entitlements, Info.plist usage strings, or signing flags? Diff them.
- Can any code path load or execute something not signed by our Team ID?
- Can any input (gateway, IPC, file) start capture without passing local policy?
