# Hexagonal ports for reliability, and per-OS adapters

The core (session engine, sync protocol, policies) depends only on these traits. Every reliability-relevant side effect sits behind a port, so it can be faked in paused-time and turmoil tests.

| Port | Responsibility | macOS adapter | Linux adapter | Windows adapter |
|---|---|---|---|---|
| `Clock` | mono/wall now, sleep, `ClockEpoch` events | tokio time + jump detector + IOKit power notifications | tokio time + jump detector + logind `PrepareForSleep` (D-Bus) | tokio time + SCM `SERVICE_CONTROL_POWEREVENT` (the jump detector is blind here because QPC includes sleep) |
| `PowerEvents` | will-sleep / did-wake | `IORegisterForSystemPower` on a CFRunLoop thread | logind inhibitor + `PrepareForSleep` | service `HandlerEx` power events, or `RegisterSuspendResumeNotification` for apps |
| `NetworkMonitor` | path up/down/changed | `NWPathMonitor` (Network.framework) | netlink / NetworkManager D-Bus | `NotifyIpInterfaceChange` / NLM |
| `GatewayTransport` | connect (any AsyncRead+Write), TLS, WS framing | tokio TCP + rustls (platform verifier) | same | same |
| `Storage` / `Outbox` | durable state, outbox, retention, backup | rusqlite writer thread | same | same (bundled SQLite; mind AV scanners locking files [secondary]) |
| `SecretStore` | get/set/rotate tokens | Keychain (`apple-native-keyring-store`) | Secret Service (`zbus-secret-service-keyring-store`) or a file with 0600 permissions for headless use | Credential Manager (`windows-native-keyring-store`) |
| `AudioSource` / `WakeWord` | frames into a ring; detections out | CoreAudio callback into an SPSC ring; engine in a helper process | ALSA/PipeWire | WASAPI |
| `Supervisor` | crash ledger, readiness, watchdog ping, stop signal | launchd LaunchAgent (KeepAlive, ThrottleInterval 10 s, SIGTERM) | systemd user service `Type=notify`, `WatchdogSec`, `sd_notify` | SCM service (windows-service) with recovery actions; or a per-user logon task for mic/user context |
| `HealthSink` | publish health and metrics | gateway heartbeat + log | same + optional journald | same + Event Log |
| `ConfigSource` | load, validate, watch | file + FSEvents (notify) + periodic re-read | inotify (notify) + SIGHUP | ReadDirectoryChangesW (notify) |

## Key OS differences

- **Sleep semantics**: macOS/Linux `Instant` stops during sleep. Windows `Instant` advances. The macOS lid-closed DarkWake/Power Nap may briefly run the process without user-visible wake [secondary]; do not treat it as "awake and online".
- **Supervision**: launchd has no restart limit and only a 10 s throttle. systemd has growing restart delays (`RestartSteps`/`RestartMaxDelaySec`) and a start-rate limit. Windows SCM has recovery actions with a reset period. The in-app crash-loop breaker makes behavior uniform across all three.
- **Stop signal**: launchd and systemd send SIGTERM, then SIGKILL after a timeout. On Windows the stop comes as an SCM control event, and the service must report `STOP_PENDING` with wait hints. Console apps get `CTRL_CLOSE_EVENT`/`CTRL_SHUTDOWN_EVENT` (tokio has `tokio::signal::windows`) [secondary].
- **Watchdog**: only systemd has a native one (`WatchdogSec`). On macOS and Windows the in-process OS-thread watchdog plus `abort()` is the watchdog.
- **fd/handle limits**: macOS `launchctl limit maxfiles` / plist `NumberOfFiles`. Linux `LimitNOFILE=`. Windows handles are effectively unlimited, so watch the handle count (`GetProcessHandleCount`) instead [secondary].
- **Per-user context**: microphone permission (TCC on macOS) and the user keychain require a per-user agent (LaunchAgent, a systemd **user** unit, or a Windows logon task or user-session process). A system service lacks them [secondary].
