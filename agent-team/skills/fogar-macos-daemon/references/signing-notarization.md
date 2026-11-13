# Code signing, hardened runtime, notarization, stapling (Rust binaries in a Developer ID app)

Untested here. Tags as in launchd-smappservice.md.

## Hard requirements for notarization [primary: "Notarizing macOS software before distribution"]
- Sign every executable with a **Developer ID Application** certificate (not ad hoc, not Apple Development).
- Enable the **Hardened Runtime** for app *and* command-line targets (`codesign -o runtime`).
- Include a **secure timestamp** (`--timestamp`).
- **No `com.apple.security.get-task-allow`** set to true. Cargo/rustc never adds it, but check after signing.
- Entitlements must be XML, ASCII. Quinn adds: LF line endings, no comments, no BOM, normalise with `plutil` [secondary].
- Use `notarytool` (altool has been rejected since 2023-11-01). Notarizable containers: apps, non-app bundles, UDIF disk images, flat installer packages. A zip can be *submitted* but **cannot be stapled** [primary + secondary: Quinn "Packaging Mac Software for Distribution"].

## Entitlements for fogar-agent (hardened runtime "resource access") [primary: Hardened Runtime doc]
| Need | Entitlement on the **agent executable** | Info.plist purpose string (agent's bundle) |
|---|---|---|
| Microphone | `com.apple.security.device.audio-input` | `NSMicrophoneUsageDescription` |
| Camera (if ever) | `com.apple.security.device.camera` | `NSCameraUsageDescription` |
| Apple Events (if ever) | `com.apple.security.automation.apple-events` | `NSAppleEventsUsageDescription` |
| Screen recording, Accessibility | **no entitlement**; TCC only | — |

- "You add entitlements only to executables. Shared libraries… inherit" [primary]. Quinn: applying entitlements to library code "can prevent your code from running" [secondary].
- Avoid `com.apple.security.cs.disable-library-validation` and `allow-dyld-environment-variables`. Rust static binaries do not need them.
- `com.apple.developer.persistent-content-capture` (to skip the recurring Sequoia screen-recording re-approval) is a **restricted** entitlement. Apple's docs say it is for VNC-style apps, via a request form [secondary: mjtsai 2024-08]. A restricted entitlement needs a provisioning profile, which means the executable must live in an **app-like bundle** (Quinn, "Signing a Daemon with a Restricted Entitlement") [secondary]. This is one more reason to wrap the agent as `FogarAgent.app`.

## Signing order (inside-out, never `--deep`) [secondary: Quinn "Creating Distribution-Signed Code for Mac", forum 701514]
"Sign code from the inside out." "do not pass the --deep option to codesign."
```
# 1. Rust binary inside the helper app (built for aarch64-apple-darwin; universal via lipo if Intel needed)
codesign -s "Developer ID Application: <NAME> (<TEAMID>)" -f --timestamp -o runtime \
  --entitlements agent.entitlements  Fogar.app/Contents/Helpers/FogarAgent.app
#   (bundle path, so the identifier = CFBundleIdentifier com.fogar.agent; for a BARE binary add -i com.fogar.agent)
# 2. Sparkle's nested helpers/XPC services (Autoupdate, Updater.app, Downloader/Installer XPC) – each, inside-out
# 3. Main app last
codesign -s "Developer ID Application: …" -f --timestamp -o runtime --entitlements app.entitlements Fogar.app
codesign --verify --strict --verbose=2 Fogar.app          # [secondary]
codesign -d --entitlements - --xml Fogar.app/Contents/Helpers/FogarAgent.app | plutil -convert xml1 -o - -
```
- **Identifier stability:** keep `com.fogar.agent` (and the Team ID) identical in every release. Changing it broke an SMAppService daemon on upgrade with a launch-constraint violation [secondary: forum 795022].
- Rust specifics [unverified, check]: build with `--release`; `strip` **before** signing (stripping after signing invalidates the signature); set `MACOSX_DEPLOYMENT_TARGET` (≥ 13.0 for SMAppService) so the minimum OS in the Mach-O matches the app.
- Embedding Info.plist into a *bare* binary (only if not using the app-like wrapper): linker arg `-sectcreate __TEXT __info_plist Info.plist` from build.rs (`cargo:rustc-link-arg=…`), or the `embed_plist` crate (1.2.2, 2022) [secondary: Apple engineer in forum 111100 describes "Create Info.plist Section in Binary"].

## Packaging and notarizing
```
ditto -c -k --keepParent Fogar.app Fogar.zip                      # for submission / Sparkle archive
xcrun notarytool submit Fogar.zip --keychain-profile fogar-notary --wait
xcrun notarytool log <submission-id> --keychain-profile fogar-notary   # ALWAYS read warnings
xcrun stapler staple Fogar.app                                    # staple the APP (or the DMG), not the zip
# DMG route: hdiutil create -srcFolder dist -o Fogar.dmg; codesign -s "Developer ID Application…" --timestamp Fogar.dmg; notarize dmg; stapler staple Fogar.dmg
spctl -a -vvv -t exec Fogar.app                                   # Gatekeeper assessment [secondary]
```
- Notarize the **outermost** container. Its nested content is covered [secondary].
- Stapling is "recommended but not mandatory". Without it, Gatekeeper may block offline first launch [secondary].
- Sparkle: archives must contain the stapled app. Sign updates with Sparkle's EdDSA key (separate from Apple signing) [secondary: Sparkle docs, not fetched in this session → verify].

## CI
- Signing and notarization need macOS runners (Xcode's `codesign`, `notarytool`). `rcodesign` (apple-codesign crate) can sign and notarize from Linux [unverified, not researched in depth; evaluate before relying on it].
- Store the notary credential as an App Store Connect API key (`notarytool store-credentials`) [secondary].

## Windows / Linux equivalents
- Windows: Authenticode (signtool) with an EV/OV cert or Azure Trusted Signing. SmartScreen reputation stands in for notarization. MSIX if packaging [unverified details].
- Linux: no OS-level signing. Sign the packages (deb/rpm repo keys) or AppImage signatures. Cost: low.
