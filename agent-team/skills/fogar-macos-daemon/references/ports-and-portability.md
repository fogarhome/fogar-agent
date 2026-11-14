# Hexagonal ports for the daemon shell, and the per-OS adapter matrix

## Crate layout
```
fogar-protocol     (no I/O; serde types, framing, versions; #[typeshare])     ← shared with UI codegen
fogar-core         (no OS deps; domain + ports as traits + supervisor, crash-loop breaker, update handshake, migrations)
fogar-adapter-macos   (objc2*, IOKit, Security, os_log; ALL unsafe lives here)   cfg(target_os="macos")
fogar-adapter-linux   (systemd/sd_notify, SO_PEERCRED, logind/UPower via D-Bus) cfg(target_os="linux")
fogar-adapter-windows (named pipes, power notifications, ETW/file log)           cfg(windows)
fogar-agent        (binary: picks adapters, owns main thread/run loop, starts tokio on a background thread)
```
Rule: `fogar-core` must compile and its tests must pass on Linux CI with **fake adapters**. A `cargo deny`/`cargo tree` check forbids `objc2*`, `windows*`, `zbus` in core.

## Ports (traits in core; async via `async fn` in traits or boxed futures; all `Send + Sync`)

| Port | Responsibility (core view) | macOS adapter | Linux adapter | Windows adapter |
|---|---|---|---|---|
| `ServiceHost` | `shutdown_signal()`, `notify_ready()`, `watchdog_ping()`, `exit(code: ExitIntent)` where `ExitIntent = Stop \| RestartForUpdate \| Fatal` maps to exit codes | SIGTERM via tokio signal; no ready/watchdog; exit codes for launchd KeepAlive | SIGTERM; `sd_notify READY=1`, `WATCHDOG=1` | console ctrl/session-end events; task restart |
| `IpcTransport` | `bind() -> Listener`, `accept() -> (Conn, PeerIdentity)`, client `connect()` with server verification | UDS + `LOCAL_PEERTOKEN` + SecCode DR check | UDS in `$XDG_RUNTIME_DIR` + `SO_PEERCRED` | Named pipe + DACL + `first_pipe_instance` |
| `SystemEvents` | stream of `SleepImminent{ack}`, `Woke`, `DisplaySleep/Wake`, `PowerSource(Ac\|Battery{pct})`, `LowPower(bool)`, `Thermal(level)`, `SessionActive(bool)` | IOKit + notify(3) + NSProcessInfo | logind + UPower (D-Bus) | WM_POWERBROADCAST / power setting notifications |
| `Permissions` | `status(kind)`, `request(kind)`, events. Kinds: Microphone, ScreenCapture, Accessibility. States incl. `Unsupported`, `NeedsUserDialog` | AVFoundation / CG / AX | portals / none | privacy settings / none |
| `Paths` | state dir, cache dir, socket address, lock file | `~/Library/Application Support/com.fogar.agent`, `~/Library/Caches/…` | XDG dirs | `%LOCALAPPDATA%\Fogar` |
| `Clock`/`Timers` | monotonic time + sleep-aware scheduling | tokio + wake events | same | same |
| *(not in daemon)* `ServiceRegistrar` | register/unregister/status of the background service | **Swift UI** via SMAppService | installer / `systemctl --user` | installer / Task Scheduler |

Stays in **core**: supervisor of subsystems, crash-loop breaker and safe mode, protocol handling and auth policy ("which PeerIdentity is allowed"), update handshake decision, state migrations, back-pressure, scheduling policy that reacts to `PowerSource`/`LowPower`/`Thermal` (for example "on battery + LowPower → suspend heavy jobs").
Stays in **adapters**: every FFI call, run-loop/dispatch threading, OS paths, signature checks, OS service config files (plist/unit/task XML as checked-in templates).

## Porting cost estimate (relative, after the macOS adapter exists)
| Area | Linux | Windows |
|---|---|---|
| Service registration + lifecycle | Low (unit file, sd_notify) | Medium (logon task, no SIGTERM, file locking on update) |
| IPC transport + peer check | Low | Medium (DACL, SID) |
| System/power events | Medium (D-Bus) | Medium (message window) |
| Permissions / capture | Medium–High on Wayland (portals) | Low–Medium |
| Logging | Low (journald layer) | Low (file/ETW) |
| Signing/distribution | Low | Medium (Authenticode, SmartScreen) |

The biggest risk to portability is letting macOS concepts leak into core: dispatch queues, "ack within 30 s", or TCC-specific states. Keep the port types neutral.
