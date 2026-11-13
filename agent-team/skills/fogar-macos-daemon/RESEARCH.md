# RESEARCH: fogar-macos-daemon skill

Research date: 2026-11-12, from a Linux container. **No macOS behaviour was executed or tested.** Downloaded sources were read only, each in its own directory. Nothing from them was run.

Access limits encountered: `developer.apple.com` was blocked for curl and its HTML pages failed WebFetch. Its `.md` renderings (`…/documentation/<path>.md`) and forum threads were readable through WebFetch, which gives a model summary rather than raw text, so treat quotes as near-verbatim. `keith.github.io` (man pages), `freedesktop.org` and `learn.microsoft.com` were blocked. I fell back to the open-source launchd man page source, systemd man XML on GitHub, and crate source.

Confidence tags: **[primary]** = Apple doc/header/man page, OS source, crate source. **[secondary]** = Apple DTS/engineer forum post, reputable third party. **[unverified]** = inference, folklore, or not checked.

## 1. Sources

### Primary
| # | Source | Used for |
|---|---|---|
| P1 | `SMAppService.h` doc comments, read from **objc2-service-management 0.3.2** generated bindings (`src/generated/SMAppService.rs`) — https://crates.io/crates/objc2-service-management | agent/daemon plist location, BundleProgram, register/unregister semantics, errors, status enum, per-user registration |
| P2 | Apple doc "Updating helper executables from earlier versions of macOS" — https://developer.apple.com/documentation/servicemanagement/updating-helper-executables-from-earlier-versions-of-macos.md | bundle layout, BundleProgram, AssociatedBundleIdentifiers, Team ID match, openSystemSettingsLoginItems |
| P3 | `launchd.plist(5)` source, apple-oss-distributions/launchd tag **launchd-842.1.4** (commit dated 2021-10-06) — https://github.com/apple-oss-distributions/launchd/blob/main/man/launchd.plist.5 | KeepAlive, RunAtLoad, ThrottleInterval (10 s default), ProcessType values, LowPriorityIO, Nice, ExitTimeOut (20 s), MachServices, "MUST NOT daemonize", SIGTERM |
| P4 | xnu `bsd/sys/un.h` — https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/un.h | `sun_path[104]`, LOCAL_PEERCRED/PID/EPID/UUID/EUUID/TOKEN values |
| P5 | xnu `osfmk/mach/message.h` | `audit_token_t { unsigned int val[8]; }` |
| P6 | Apple doc "Hardened Runtime" — https://developer.apple.com/documentation/security/hardened-runtime.md | resource-access entitlement keys, entitlements only on executables |
| P7 | Apple doc "Notarizing macOS software before distribution" — https://developer.apple.com/tutorials/data/documentation/security/notarizing-macos-software-before-distribution.md | notarization requirements, notarytool, formats |
| P8 | objc2 0.6.5 source, `src/topics/run_loop.md`, `swift.md`, `rc/autorelease.rs`, `runtime/define.rs` (ProtocolBuilder) — https://github.com/madsmtm/objc2 | run-loop requirement, Swift interop options |
| P9 | dispatch2 0.3.1 source (`queue.rs`, `main_thread_bound.rs`) | DispatchQueue API names, run_on_main |
| P10 | block2 0.6.2 source | `RcBlock::new`, `StackBlock::new` |
| P11 | objc2-io-kit 0.3.2 (`pwr_mgt.rs`, `ps.rs`, `mod.rs`) | IORegisterForSystemPower contract, IOAllowPowerChange/IOCancelPowerChange, kIOPSNotify* strings, IONotificationPortSetDispatchQueue, IOPSNotificationCreateRunLoopSource |
| P12 | objc2-foundation 0.3.2 (`NSXPCConnection.rs`, `NSProcessInfo.rs`) | NSXPC API surface incl. setCodeSigningRequirement; isLowPowerModeEnabled, thermalState, notifications |
| P13 | objc2-app-kit 0.3.2 `NSWorkspace.rs` | sleep/wake/screens/session notification names |
| P14 | objc2-security 0.3.2 (`SecCode.rs`, `SecRequirement.rs`, `CSCommon.rs`) | `SecCode::copy_guest_with_attributes`, `check_validity`, `kSecGuestAttributeAudit`, `SecRequirement::create_with_string`, `SecCSFlags::DefaultFlags` |
| P15 | objc2-core-graphics / objc2-application-services / objc2-av-foundation 0.3.2 | CGPreflight/RequestScreenCaptureAccess, CGDisplayRegisterReconfigurationCallback, AXIsProcessTrusted(WithOptions), AVCaptureDevice authorization |
| P16 | tracing-oslog 0.3.0 source (`wrapper.c`, `logger.rs`, README) | `%{public}s` formatting, activity per span, level mapping |
| P17 | tokio 1.54.1 source (`net/unix/ucred.rs`, `net/windows/named_pipe.rs`) | macOS peer_cred = LOCAL_PEEREPID + getpeereid; first_pipe_instance, reject_remote_clients, create_with_security_attributes_raw |
| P18 | libc 0.2.191 (`unix/bsd/apple/mod.rs`) | SOL_LOCAL, LOCAL_PEERCRED/PID/TOKEN constants; no audit_token_t type found |
| P19 | interprocess 2.4.8 source | PeerCreds (xucred) on macOS, named pipe client_process_id |
| P20 | typeshare-core 1.13.4 source | Swift generator exists; adjacently-tagged enum requirement |
| P21 | systemd man XML (main branch) — https://github.com/systemd/systemd/tree/main/man | Restart=, RestartSec/RestartSteps, StartLimit*, WatchdogSec, Nice=, IOSchedulingClass=, CPUWeight=, MemoryHigh=, CPUQuota= |
| P22 | crates.io API (2026-11-12) | current versions of all crates named |

### Secondary (Apple DTS / engineers on developer forums; third parties)
| # | Source | Used for |
|---|---|---|
| S1 | Forum 768592 "SMAppService re-register after app upgrade" — https://developer.apple.com/forums/thread/768592 | unregister→register race (~500 ms), helper-location codesign error |
| S2 | Forum 795022 "Upgrading an SMAppService daemon and changing the plist" (Quinn) — https://developer.apple.com/forums/thread/795022 | launch-constraint violation after identifier change; no way to tell launchd about plist change |
| S3 | Forum 784801 status change notification (Quinn, May 2025) — https://developer.apple.com/forums/thread/784801 | no notification; poll status |
| S4 | Forum 763268 helper not relaunched after update (Kevin Elliott, DTS) — https://developer.apple.com/forums/thread/763268 | exit() and let launchd relaunch; atomic bundle swap; load resources early |
| S5 | Forum 775267 XPC vs AF_UNIX (Quinn) — https://developer.apple.com/forums/thread/775267 | UDS "substantially less flexible", LOCAL_PEERTOKEN "not as secure as XPC" |
| S6 | Forum 817602 UDS path (Quinn) — https://developer.apple.com/forums/thread/817602 | 104 vs 253 bytes, /var/run, NSTemporaryDirectory no guarantee |
| S7 | Forum 760972 LOCAL_PEERTOKEN (Quinn) — https://developer.apple.com/forums/thread/760972 | getsockopt code, kSecGuestAttributeAudit, don't use bundle IDs/PIDs |
| S8 | Forum 731504 responsible process (Quinn) — https://developer.apple.com/forums/thread/731504 | responsible code vs process |
| S9 | Forum 853725 daemon inheriting TCC (Quinn, ~Oct 2026) — https://developer.apple.com/forums/thread/853725 | daemons can't inherit; limited tracing even with SMAppService; app-like wrapper trade-off |
| S10 | Forum 694948 screen capture with LaunchAgent (Quinn) — https://developer.apple.com/forums/thread/694948 | TCC needs a responsible app for agents |
| S11 | Forum 805245 TCC inheritance parent→child (Quinn) — https://developer.apple.com/forums/thread/805245 | chain-walking works in tested case; exceptions exist |
| S12 | Forum 111100 CLI camera (Apple Frameworks Engineer) — https://developer.apple.com/forums/thread/111100 | Info.plist section in binary |
| S13 | Forums 701514 / 701581 Quinn "Creating Distribution-Signed Code for Mac" / "Packaging Mac Software for Distribution" — https://developer.apple.com/forums/thread/701514 , https://developer.apple.com/forums/thread/701581 | inside-out signing, no --deep, -i for non-bundled code, zip can't be signed/stapled, notarize outermost container |
| S14 | Forum 129086 notarizing CLI tool — https://developer.apple.com/forums/thread/129086 | notarizing via dmg |
| S15 | Michael Tsai, "Sequoia Screen Recording Prompts and the Persistent Content Capture Entitlement" (2024-08-08, updated) — https://mjtsai.com/blog/2024/08/08/sequoia-screen-recording-prompts-and-the-persistent-content-capture-entitlement/ ; 9to5Mac 2024-08-14 | weekly→monthly prompts, picker exemption, VNC-only entitlement, MDM key |
| S16 | Forum 766580 App Groups and macOS 15 (Quinn) — https://developer.apple.com/forums/thread/766580 | pointer to "App Groups: macOS vs iOS: Fight!" (not read directly) |
| S17 | StackOverflow (kiwix mirror) "await SCShareableContent never returns" | leaked SCStream hypothesis (weak) |

## 2. Claim ledger (non-trivial claims)

| Claim | Confidence | Source |
|---|---|---|
| Agent plist must be in `Contents/Library/LaunchAgents`; `BundleProgram` bundle-relative allowed | primary | P1, P2 |
| Agents registered per user, only from within that user's context | primary | P1 |
| register() errors: AlreadyRegistered, LaunchDeniedByUser, InvalidSignature | primary | P1 |
| status values 0..3 | primary | P1 |
| Legacy plists need AssociatedBundleIdentifiers and matching Team ID | primary | P2 |
| No status-change notification; poll | secondary | S3 |
| status/register latency or blocking | **unverified** | none found |
| Default throttle 10 s; ProcessType semantics; ExitTimeOut 20 s | primary (2021 OSS man page; current macOS may differ) | P3 |
| launchd never gives up on crash loops | unverified (absence of evidence in P3) | P3 |
| KeepAlive{SuccessfulExit=false} restarts on signal death | unverified | — |
| ProcessType Adaptive tied to XPC transactions | primary | P3 |
| Identifier change → AMFI launch constraint violation on SMAppService upgrade | secondary (single case) | S2 |
| Old helper keeps running after update; exit and let launchd relaunch | secondary | S4 |
| unregister→register race needs delay | secondary (single report) | S1 |
| sun_path 104; LOCAL_PEERTOKEN = 6 | primary | P4, P18 |
| LOCAL_PEERTOKEN identifies socket creator, not sender; weaker than XPC | secondary | S5 |
| tokio peer_cred on macOS lacks audit token | primary | P17 |
| SecCode guest lookup by audit token API names in objc2-security | primary (names); usage untested | P14 |
| No maintained Rust XPC crate; objc2 has no XPC crate | primary (crates.io/objc2 listing 2026-11-12) | P8, P22 |
| NSXPC from Rust blocked by extended block signatures | **unverified** (reasoned) | P8 ProtocolBuilder |
| audio-input entitlement required under hardened runtime for mic | primary (key listed) / exact enforcement unverified | P6 |
| Daemons cannot inherit app TCC; agent tracing limited even with SMAppService | secondary | S9 |
| App-like wrapper gives own TCC identity | secondary (trade-off described) / prompt behaviour unverified | S9 |
| Missing usage string → TCC crash | secondary/unverified | S12 + forum titles |
| Sequoia monthly screen-recording re-approval; picker exempt | secondary | S15 |
| Inside-out signing, no --deep, -i for bare binaries | secondary (DTS) | S13 |
| Zip cannot be stapled | secondary | S13 |
| tracing-oslog logs `%{public}s`, activity per span | primary | P16 |
| IORegisterForSystemPower ack contract (30 s) | primary | P11 |
| Dispatch-queue delivery for IONotificationPort | primary (API exists) / behaviour unverified | P11 |
| notify_register_dispatch not bound in libc/dispatch2 | primary (grep) | P18, P9 |
| Autorelease leak on pool-less threads | unverified (standard Cocoa knowledge) | — |
| typeshare requires adjacently tagged enums | primary | P20 |
| systemd Restart/StartLimit/RestartSteps semantics | primary | P21 |
| tokio named pipe hardening options | primary | P17 |
| Windows services run in session 0 without desktop/audio access | unverified (well known, MS docs not reachable) | — |

## 3. Conflicts and tensions between sources
1. **Helper location.** Apple's doc example uses `Contents/Resources/mydaemon` (P2). Quinn's signing guidance treats bare tools as non-bundled code. One forum user saw register() fail with `-67028` from `Contents/Helpers` (S1), and another found TCC "inheritance" only from `Contents/MacOS` (S9, Quinn "surprised"). → The skill recommends an app-like wrapper and makes the exact path a blocking verification item.
2. **UDS vs XPC.** DTS consistently prefers XPC (S5, S7). The project's portability goal and Rust tooling favour UDS. → UDS is chosen with the residual risk documented, and XPC is kept as a possible future adapter.
3. **unregister completion semantics.** The header says re-register is safe after the completion handler (P1). A forum report says it fails without a delay (S1). → retry/backoff.
4. **ProcessType default.** The man page says unset means "light resource limits… throttling" while `Standard` is "equivalent to no ProcessType" (P3). How this interacts with QoS on Apple Silicon P/E cores is not documented. → measure.
5. **Man page age.** P3 is the newest open-source launchd (842.1.4, 2021). Shipping macOS man pages may document more keys (e.g. `AssociatedBundleIdentifiers`, `BundleProgram`, possibly `Crashed` under KeepAlive). This was not verifiable here.

## 4. Open questions
- Exact SMAppService behaviour for an agent whose `BundleProgram` points into a nested `.app`, and how TCC attributes it (Fogar vs Fogar Agent).
- Does ScreenCaptureKit require an `NSApplication`/WindowServer connection in an LSUIElement agent?
- macOS 26 Tahoe: any changes to BTM/SMAppService, TCC screen-recording cadence, or launch constraints? (Not researched; no primary source fetched.)
- Cost of `SMAppService.status` (IPC to backgroundtaskmanagementd?), so whether it is safe on the main thread.
- Real idle footprint of a tokio + objc2 binary on an M2. Effect of `ProcessType=Background` on latency of UI-triggered work.
- Is `rcodesign` (Linux signing/notarization) mature enough for CI? Not researched.
- Sparkle 2 specifics (EdDSA, helper XPC services signing, `SUEnableInstallerLauncherService`): docs not fetched.
- Windows: exact EcoQoS API, named-pipe DACL recipe, Task Scheduler restart semantics (MS docs blocked).
- Linux: zbus/logind inhibitor crate choice (not researched).

## 5. Verify on hardware (consolidated test plan)
1. Build `Fogar.app` with the nested `FogarAgent.app` and plist. Then `register()` → `launchctl print gui/$UID/com.fogar.agent` shows it running. Run `sfltool dumpbtm`.
2. Kill -9, then panic (SIGABRT), then exit 75, then exit 0. Record which ones restart and the delay.
3. Crash loop for 10 min: confirm the throttle interval and confirm the safe-mode breaker engages.
4. Mic prompt and grant from the agent with the UI closed. Screen recording grant, and whether a restart is needed. Accessibility grant. Repeat after a Sparkle update and after moving the app to another folder.
5. UDS mutual verification: correct app passes, an ad-hoc-signed impostor fails, a dev build follows the dev requirement path. Check the client-side token of the server.
6. Sleep/wake 50 cycles: every WillSleep acked < 1 s, no missed HasPoweredOn, no leaked memory.
7. 72 h soak on battery: CPU, idle wakeups, `footprint`, os_log volume, `powermetrics` energy impact. Compare Background vs Standard.
8. Notarize, staple, then a quarantined download on a clean macOS 14/15/26 VM → Gatekeeper accepts and the Login Items entry shows "Fogar".
9. Upgrade from version N to N+1 with a changed plist key: unregister/register path works with retries, and permissions are retained.
