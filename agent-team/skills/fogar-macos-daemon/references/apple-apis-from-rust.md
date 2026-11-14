# Calling Apple APIs from a tokio daemon (objc2 family), logging, crashes, system events

Versions checked on crates.io on 2026-11-12: objc2 0.6.5, block2 0.6.2, dispatch2 0.3.1, objc2-foundation / objc2-app-kit / objc2-io-kit / objc2-core-foundation / objc2-security / objc2-service-management / objc2-screen-capture-kit / objc2-core-graphics / objc2-application-services / objc2-av-foundation 0.3.2, core-foundation 0.10.1 (older servo crate; prefer objc2-core-foundation), tracing-oslog 0.3.0, oslog 0.2.0 (2022), objc2-os-log 0.3.2. **Pin exact versions and re-check them before use.**

## 1. Threading model (the core pattern)

```
main thread:    platform event loop (CFRunLoop / NSRunLoop, or NSApplication if SCK requires it)
                  └ only here: anything that "assumes the main run loop is running"
tokio runtime:  started on a background thread (Builder::new_multi_thread().worker_threads(2), or current_thread)
                  └ all business logic, IPC, timers
Apple callbacks: delivered on a private serial DispatchQueue (dispatch2::DispatchQueue::new("com.fogar.agent.os", ..))
                  └ handler converts to a plain Rust enum and does tx.try_send(event) into a tokio mpsc
Rust → Apple calls: from tokio via spawn_blocking or by exec_async on the private queue; never block a tokio worker
```
- objc2: "A lot of things in various different frameworks assume that the main thread's run loop is currently running, and will block indefinitely or fail in confusing ways if it is not." Non-graphical apps should run `NSRunLoop::currentRunLoop()`. A lower level `dispatch_main`/`CFRunLoop` "may prevent things that depend on NSRunLoop features from working, so test thoroughly" [primary: objc2 topics/run_loop.md]. → **Keep the main thread for the Apple run loop and move tokio off it.** `#[tokio::main]` takes over the main thread, so do not use it in the macOS adapter binary.
- `dispatch2`: `DispatchQueue::new(label, attr)`, `exec_async`, `exec_sync`, `after`, `DispatchQueue::main()`, `dispatch_main()`, `run_on_main(|mtm| …)` [primary: dispatch2 0.3.1 source].
- `MainThreadMarker` (objc2) proves main-thread access at compile time. Main-thread-only APIs require it [primary].
- Blocks: `block2::RcBlock::new(closure)` for heap blocks that Apple stores (completion handlers), and `StackBlock::new` for synchronous use [primary: block2 0.6.2].

## 2. Safety patterns
- **Autorelease pools:** wrap each batch of Objective-C calls made on tokio or other non-Apple threads in `objc2::rc::autoreleasepool(|_| { … })`. Threads without a pool hold autoreleased objects until the thread exits [unverified for modern runtime behaviour, standard Cocoa rule]. Over a multi-day run this shows up as slow memory growth. GCD queues drain their own pools per work item [unverified].
- Keep all `unsafe` objc2/IOKit calls inside the `fogar-macos` adapter crate. Expose safe, `Send` Rust types to core. Core never sees `Retained<…>`.
- Callbacks must not panic across the FFI boundary. objc2 bindings use `extern "C-unwind"` [primary], but wrap handler bodies in `catch_unwind` anyway and log.
- Callback context pointers (`refcon`) must point to a heap allocation (`Box::into_raw`) that lives until deregistration. Deregister in the order the docs specify.
- Never block inside a power callback beyond the ack deadline (30 s, see below).

## 3. System events → `SystemEvents` port

| Event | Recommended API (all bindings exist in objc2-io-kit 0.3.2) | Delivery without a main run loop |
|---|---|---|
| Will sleep / has powered on / can sleep / will not sleep | `IORegisterForSystemPower(refcon, &mut port_ref, callback, &mut notifier)` → `io_connect_t` [primary] | `IONotificationPortSetDispatchQueue(port, queue)` (binding exists) [primary]. Otherwise `IONotificationPortGetRunLoopSource` |
| Power source change (AC↔battery) | `IOPSNotificationCreateRunLoopSource(cb, ctx)` (needs a CFRunLoop) **or** notify(3) key `kIOPSNotifyPowerSource` = `"com.apple.system.powersources.source"` [primary: constant], then `IOPSCopyPowerSourcesInfo` + `IOPSGetProvidingPowerSourceType` | notify(3) `notify_register_dispatch` is **not bound in libc/dispatch2**. Write a 5-line `extern "C"` + block2 shim [primary: grep; unverified shim] |
| Low Power Mode, thermal state | `NSProcessInfo::isLowPowerModeEnabled`, `thermalState`; notifications `NSProcessInfoPowerStateDidChangeNotification`, `NSProcessInfoThermalStateDidChangeNotification` (objc2-foundation) [primary] | NSNotificationCenter, delivered on the posting thread [unverified] |
| Display sleep/wake, session active/resign | `NSWorkspaceScreensDidSleepNotification`, `…ScreensDidWake…`, `…SessionDidBecomeActive…`, `…SessionDidResignActive…` via `NSWorkspace.notificationCenter` (objc2-app-kit) [primary: statics exist] | Needs a running main run loop and AppKit linked [unverified] |
| Display reconfiguration | `CGDisplayRegisterReconfigurationCallback` (objc2-core-graphics) [primary] | Requires a run loop / WindowServer connection [unverified] |

IORegisterForSystemPower contract (header text in objc2-io-kit) [primary]:
- `kIOMessageSystemWillSleep`: "Callers MUST acknowledge this event by calling IOAllowPowerChange". Without an ack, sleep continues after a 30 s timeout.
- `kIOMessageCanSystemSleep`: ack with `IOAllowPowerChange` or veto with `IOCancelPowerChange`. **fogar must never veto** idle sleep.
- `kIOMessageSystemWillPowerOn` / `kIOMessageSystemHasPoweredOn`: do NOT acknowledge. "HasPoweredOn" arrives 1–5+ s after wake. Disk and network may block before it.
- Deregister: `IODeregisterForSystemPower(notifier)`, then `IOServiceClose(connect)`, then destroy the port.

Design: on `WillSleep`, core gets `SleepImminent { ack: oneshot::Sender<()> }`. Core pauses capture, flushes state, then the adapter calls `IOAllowPowerChange`. **The adapter acks anyway after a 5 s deadline.** On `HasPoweredOn`, core re-checks permissions, reconnects resources and resets timers. Treat wall-clock jumps as normal (use `Instant` for intervals).

## 4. Energy rules for tokio on a fanless M2
- No periodic polling loops. Every timer must justify itself, and if one is needed use ≥ seconds granularity. Idle target: **zero wakeups per second** while no client is connected and nothing is capturing. Measure with `top -stats pid,cpu,idlew,power` or `powermetrics` [secondary].
- Small runtime: `worker_threads(1..=2)` or `current_thread`. Set `max_blocking_threads` low.
- Set thread QoS for blocking pools (`pthread_set_qos_class_self_np` with `QOS_CLASS_UTILITY`/`BACKGROUND`) [unverified binding in libc; check]. With `ProcessType=Background`, launchd already clamps it [unverified exact semantics].
- Measure footprint with `footprint <pid>` / `vmmap --summary` (phys_footprint is the number Activity Monitor shows) [secondary]. The 50 MB target is a hypothesis to measure, not a fact.

## 5. Unified logging (tracing → os_log)
- `tracing-oslog` 0.3.0: `OsLogger::new(subsystem, category)` is a `tracing_subscriber` layer [primary: README/source].
  - It logs every message as **`%{public}s`** [primary: wrapper.c], so all fields are visible in `log show` to anyone with admin access, and sysdiagnoses include them. **Rule: never put user content, transcripts, paths with names or tokens in log fields.** If private redaction is needed, write a custom layer that calls `os_log` with `%{private}s` (objc2-os-log 0.3.2 or a C shim).
  - It creates an **os_activity per span** [primary: logger.rs on_new_span]. Do not put spans on hot paths (per-frame/per-buffer). Use events with fields instead.
  - Level mapping: TRACE→Debug, DEBUG→Info, INFO→Default, WARN/ERROR→Error/Fault per source [primary, partial read]. Debug/Info are not persisted by default [secondary].
- Subsystem `com.fogar.agent`, categories per module (`ipc`, `power`, `capture`, `lifecycle`).
- Read logs: `log stream --predicate 'subsystem == "com.fogar.agent"' --level debug`, `log show --last 1h --predicate …` [secondary].
- Logging is not a hexagonal port. `tracing` is the facade in core, and each OS binary installs its layers: os_log (mac), `tracing-journald` (Linux), file/ETW (Windows).

## 6. Crash reports
- A Rust panic with `panic = "abort"` → SIGABRT → macOS writes an `.ips` report to `~/Library/Logs/DiagnosticReports/` (viewable in Console.app) [secondary/unverified for launchd agents]. Install a panic hook that writes the panic message to os_log (public, no user data) **before** aborting. The crash report alone does not include the Rust panic message [unverified].
- Recommendation: `panic = "abort"` in release (smaller, and with no unwinding into FFI), plus the crash-loop breaker (launchd-smappservice.md §5).
- Symbolication: keep the `dSYM` for every shipped build (`CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=packed` produces a `.dSYM` on macOS) [unverified flag behaviour; check cargo docs].
- Optional later: MetricKit `MXCrashDiagnostic` (objc2-metric-kit exists) or a self-hosted reporter. Not in v1.

## 7. Windows / Linux equivalents for SystemEvents
- Linux: logind D-Bus `PrepareForSleep(bool)` + inhibitor locks ("delay" mode) to get the same ack semantics. UPower D-Bus `OnBattery`. Crates: zbus [unverified versions]. Cost: medium.
- Windows: `RegisterPowerSettingNotification` / `PowerRegisterSuspendResumeNotification`, `WM_POWERBROADCAST` (`PBT_APMSUSPEND`, `PBT_APMRESUMEAUTOMATIC`), `GUID_ACDC_POWER_SOURCE`. Needs a hidden message window or callback. Crate: `windows` [unverified]. Cost: medium.
