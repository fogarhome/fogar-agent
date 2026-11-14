# launchd + SMAppService for fogar-agent (macOS)

Status: researched 2026-11-12 on Linux. **Nothing here was tested on a Mac.** Tags: [primary] Apple header/man page/doc or crate source; [secondary] Apple DTS forum post or reputable third party; [unverified] inference or folklore.

## 1. Decision: SMAppService.agent, not a hand-installed plist

| Option | Use when | Why / why not |
|---|---|---|
| **`SMAppService.agent(plistName:)` with the plist inside the app bundle** | **Default for fogar-agent** (macOS 13+) | No installer script. Shows up under the app's name in System Settings › Login Items. User can toggle it. Survives the user moving the app (`BundleProgram` is bundle-relative) [primary: SMAppService.h via objc2-service-management 0.3.2] |
| Legacy `~/Library/LaunchAgents/<label>.plist` + `launchctl bootstrap` | Dev loops only; MDM installs | Must add `AssociatedBundleIdentifiers`, otherwise System Settings shows the signing org name. The Team ID of the program must match the app [primary: "Updating helper executables…" doc] |
| `SMAppService.daemon` (LaunchDaemon, root) | Never for v1 | Needs admin approval. Runs outside the user session, so TCC gives it no microphone or screen access. Quinn: "it's not possible for a daemon to inherit its container app's TCC privileges" [secondary: forum 853725] |
| `SMAppService.mainApp` / `loginItem(identifier:)` | Not suitable | A login item is not restarted by `KeepAlive`, and you get no launchd keys |

Agents are per user. "If an app desires to register a LaunchAgent for multiple users, the API must be called once per user while the desired user is running the app. LaunchAgents cannot be registered from outside a user context." [primary: header]

## 2. Bundle layout (target)

```
Fogar.app/
  Contents/
    Info.plist                      (UI app, CFBundleIdentifier = com.fogar.app)
    MacOS/Fogar                     (SwiftUI app)
    Library/LaunchAgents/com.fogar.agent.plist   <- SMAppService looks here [primary]
    Helpers/FogarAgent.app/         <- RECOMMENDED app-like wrapper for the daemon (see tcc-permissions.md)
      Contents/Info.plist           (CFBundleIdentifier com.fogar.agent, LSUIElement/LSBackgroundOnly, usage strings)
      Contents/MacOS/fogar-agent    (Rust binary)
    Frameworks/Sparkle.framework
```
- The documented example puts the bare helper at `Contents/Resources/mydaemon` [primary]. Quinn's signing posts treat a bare tool as "non-bundled code" that needs `codesign -i` [secondary]. In one forum report (768592), a helper copied to `Contents/Helpers` failed `register()` with a code-signing error (-67028) [secondary]. **Verify on a Mac which location `register()` accepts for an app-like helper.** If `Contents/Helpers/…app` fails, try `Contents/Library/LoginItems/FogarAgent.app` and set `BundleProgram` to it.
- Sign the main app, the helper app and every nested binary with the **same Team ID** [primary for legacy plists; also required for SMAppService signature checks, which fail with `kSMErrorInvalidSignature`].
- **Keep the helper's code-signing identifier and Team ID stable across all versions.** In forum 795022, changing the daemon's identifier between versions caused `AMFI: Launch Constraint Violation` on upgrade. The only fix was to restore the old identifier [secondary]. TCC grants are also keyed to the designated requirement, so changing the identifier would reset permissions [unverified, consistent with TN3127].

## 3. The plist (starting point, measure before tuning)

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.fogar.agent</string>
  <key>BundleProgram</key><string>Contents/Helpers/FogarAgent.app/Contents/MacOS/fogar-agent</string>
  <key>ProgramArguments</key><array><string>fogar-agent</string><string>--launchd</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>30</integer>
  <key>ProcessType</key><string>Background</string>
  <key>LowPriorityIO</key><true/>
  <key>Nice</key><integer>5</integer>
  <key>ExitTimeOut</key><integer>10</integer>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
</dict></plist>
```
Meaning of each key, quoted from launchd.plist(5) (open-source launchd-842.1.4, 2021 tag; the man page on current macOS may add keys) [primary unless marked]:

| Key | Semantics | fogar choice |
|---|---|---|
| `RunAtLoad` | "launched once at the time the job is loaded. The default is false." | true. SMAppService also bootstraps the agent at registration and at each login [primary: header] |
| `KeepAlive` | Default false. `true` = run unconditionally. Dict keys are ORed. "Jobs that exit quickly and frequently when configured to be kept alive will be throttled". | `{SuccessfulExit=false}`: restart after a crash or non-zero exit, and stay down after exit(0). This gives a clean "stop" path. Also `SuccessfulExit` "implies that RunAtLoad is set to true". Whether death by a signal (panic=abort gives SIGABRT) counts as an unsuccessful exit is [unverified]. Test it. |
| `ThrottleInterval` | "by default, jobs will not be spawned more than once every 10 seconds" | 30 s caps a crash loop at 2 spawns per minute. **launchd has no burst limit or give-up**, so a crash loop runs forever [primary: absence in man page; unverified for current macOS] |
| `ProcessType` | Unset = "light resource limits… throttling its CPU usage and I/O bandwidth". `Background` = limits "to prevent them from disrupting the user experience". `Adaptive` = moves between Background and Interactive "based on activity over XPC connections" (needs XPC transactions, **so it is useless with a Unix socket**). `Interactive` = no limits; "should only be used if an app's ability to be responsive depends on it" | `Background` on the fanless M2. If UI request latency suffers, measure `Standard` instead. Effect on P/E-core placement is [unverified] |
| `LowPriorityIO` | "the kernel should consider this daemon to be low priority when doing file system I/O" | true |
| `Nice` | nice(3) value | 5. Redundant with Background, harmless [unverified effect] |
| `ExitTimeOut` | Time between SIGTERM and SIGKILL; "default value is 20 seconds" | 10. The daemon must handle SIGTERM: flush, close the socket, exit(0) |
| `LimitLoadToSessionType` | Restrict to a session type | `Aqua` = GUI login session only. That is where TCC, screen and audio make sense. Whether SMAppService honours this key for agents is [unverified] |
| `AbandonProcessGroup` | default: launchd kills the whole process group when the job dies | leave unset. Child processes die with the agent, which is what we want |
| `EnableTransactions`, `MachServices`, `Sockets` | XPC/Mach and launch-on-demand | not used in v1 (see ipc.md) |

Required daemon behaviour (launchd.plist(5), "A daemon or agent launched by launchd MUST NOT… fork/daemonize, setsid, close stray fds, redirect stdio…" and "SHOULD catch SIGTERM") [primary]: **never daemonize, never double-fork, stay in the foreground, and handle SIGTERM.**

Do not set `StandardOutPath` or `StandardErrorPath` in production. Log to os_log instead (see apple-apis-from-rust.md). Unbounded stdout files grow for days.

## 4. Registration flow (lives in the Swift UI app, not in the daemon)

1. Call `SMAppService.agent(plistName: "com.fogar.agent.plist")` at app launch, **off the main thread** [unverified cost: status/register go through IPC to system daemons. No doc states their latency, so measure it].
2. Read `status`. The enum values are `NotRegistered = 0`, `Enabled = 1`, `RequiresApproval = 2`, `NotFound = 3` [primary: objc2-service-management `SMAppServiceStatus`].
   - `.notRegistered` → `register()`. Errors: `kSMErrorAlreadyRegistered`, `kSMErrorLaunchDeniedByUser`, `kSMErrorInvalidSignature` [primary: header].
   - `.requiresApproval` → explain to the user, then `SMAppService.openSystemSettingsLoginItems()` [primary].
   - `.notFound` → the bundle is broken or the plist name is wrong. Log it and show an error.
3. There is **no change notification**. Poll `status` when the app becomes active. Quinn: "polling the SMAppService.status property is going to be a lot easier" [secondary: forum 784801].
4. Unregister: prefer `unregisterWithCompletionHandler` (async; "safe to re-register" after the handler runs) [primary]. In practice an immediate re-`register()` was seen to fail with "Operation not permitted" without a ~500 ms delay [secondary: forum 768592], so retry with backoff.
5. Rust side, if ever needed: `objc2-service-management` 0.3.2 exposes `SMAppService::agentServiceWithPlistName`, `registerAndReturnError`, `unregisterAndReturnError`, `unregisterWithCompletionHandler`, `status`, `openSystemSettingsLoginItems` (all `unsafe`) [primary: crate source]. Prefer keeping registration in Swift.

## 5. Crash loops: defend inside the daemon

launchd only rate-limits restarts. Implement a **crash-loop breaker in core**:
- At start, append a timestamp to `state/starts.log` (bounded ring of 16 entries).
- If there are ≥5 starts in 10 min, enter **safe mode**: stay alive and keep serving IPC (status/diagnostics only), disable the subsystems that crashed, report to the UI, and do not exit. Exiting would re-enter the throttle loop and burn energy.
- Clear the safe-mode flag on an explicit user action or a new app version.

## 6. Debugging commands (run by a human on a Mac)

- `launchctl print gui/$(id -u)/com.fogar.agent`: state, last exit status, spawn count [secondary].
- `sfltool dumpbtm`: Background Task Management database, shows what SMAppService registered [secondary].
- `log stream --predicate 'subsystem == "com.fogar.agent"' --level debug`.
- Reset approval state during development: unregister, then delete the app and reinstall. In forum 795022 the manual workaround was "disable in Login Items, delete app, empty Trash, wait, reinstall" [secondary].

## 7. Windows / Linux equivalents

| macOS | Linux | Windows |
|---|---|---|
| LaunchAgent (per-user, GUI session) | **systemd user unit** `~/.config/systemd/user/fogar-agent.service`, `WantedBy=default.target` (or `graphical-session.target`) | **Per-user process at logon**: Task Scheduler "At log on" task (restart-on-failure settings) or `HKCU\…\Run`. **Not** a Windows service, because services run in session 0 and cannot reach the user's desktop, screen or audio session [unverified details] |
| `KeepAlive{SuccessfulExit=false}` | `Restart=on-failure` | Task Scheduler restart count/interval. A Run key has no restart, so use a tiny watchdog or the task |
| `ThrottleInterval` (no burst cap) | `RestartSec=`, `RestartSteps=`/`RestartMaxDelaySec=` (exponential), `StartLimitIntervalSec=`/`StartLimitBurst=` (gives up) [primary: systemd man sources] | task restart interval and count |
| `ProcessType=Background`, `LowPriorityIO`, `Nice` | `Nice=`, `IOSchedulingClass=idle`, `CPUWeight=` (1–10000 or `idle`), `MemoryHigh=`, `CPUQuota=` [primary] | Process priority class `BELOW_NORMAL`/`IDLE`, EcoQoS via `SetProcessInformation(ProcessPowerThrottling)` [unverified names, check MS docs] |
| SIGTERM + `ExitTimeOut` | SIGTERM + `TimeoutStopSec=`; `Type=notify` + `WatchdogSec=` + sd_notify | WM_QUERYENDSESSION / console ctrl events for a user process |
| SMAppService register (from the app) | `systemctl --user enable --now` (from the installer or the UI) | Create the task (installer/MSIX startup task) |

Porting cost: low. The plist, unit file and task XML are data. The core `ServiceHost` port (shutdown signal, readiness, optional watchdog ping) is about 100 lines per adapter.
