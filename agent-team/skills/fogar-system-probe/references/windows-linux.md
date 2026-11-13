# Windows and Linux adapters: equivalents and porting cost

Core (classifier, smoothing, policy, scheduler, report, lease logic) is reused 100%.
Only `SystemProbe`, `ProcessInspector`, `DelegatedJobs` adapters change.

## Mapping table

| Capability | macOS | Linux | Windows |
|---|---|---|---|
| CPU total/per core | `host_processor_info` ticks | `/proc/stat` (`cpu` / `cpuN` lines, USER_HZ) — 5 µs/read [tested here] | `GetSystemTimes` (idle/kernel/user; **kernel includes idle** → busy = (k+u−i)/(k+u)) [primary MS doc]; or PDH `\Processor Information(_Total)\% Processor Utility` (matches Task Manager) [secondary] |
| CPU contention | — (no PSI) | **PSI** `/proc/pressure/cpu` `some avg10/60/300 total` — 2.5 µs/read [tested] | — (Processor Queue Length counter) |
| Memory pressure (event) | dispatch MEMORYPRESSURE | **PSI trigger**: write `"some 150000 2000000"` to `/proc/pressure/memory`, `poll(POLLPRI)` [primary kernel doc + tested] | `CreateMemoryResourceNotification(LowMemoryResourceNotification)` → waitable handle [primary MS doc] |
| Memory level | `kern.memorystatus_level` | `/proc/meminfo` MemAvailable; PSI memory `full` | `GlobalMemoryStatusEx().dwMemoryLoad` |
| Swap/thrash | vm_statistics64 swapouts | `/proc/vmstat` pswpout; PSI memory `full` | PDH `\Memory\Pages/sec` |
| I/O stall | — | PSI `/proc/pressure/io` | PDH disk queue |
| Thermal | notify thermal pressure / NSProcessInfo | `/sys/class/thermal/thermal_zone*/temp` + `trip_point_*` (poll) [secondary] | **no reliable public API** (WMI `MSAcpi_ThermalZoneTemperature` needs admin, often absent) [secondary] → Unavailable |
| AC / battery | IOPS + notify | `/sys/class/power_supply/*/{online,capacity,status}`, UPower over D-Bus (events) | `GetSystemPowerStatus` (`ACLineStatus`, `BatteryLifePercent`, `SystemStatusFlag`=battery saver) + `RegisterPowerSettingNotification(GUID_ACDC_POWER_SOURCE / GUID_POWER_SAVING_STATUS)` [primary MS doc] |
| Low-power mode | `isLowPowerModeEnabled` | power-profiles-daemon `ActiveProfile == "power-saver"` (D-Bus) [secondary] | Battery saver flag; `PowerRegisterForEffectivePowerModeNotifications` (Win10 1809+) [primary MS doc] |
| User idle | HIDIdleTime | logind `IdleHint`/`IdleSinceHint` (only as good as the DE sets it) or compositor idle-notify protocols [secondary] | `GetLastInputInfo` — **session-specific**; a service in session 0 cannot see the user → run a per-user agent process [primary MS doc] |
| User busy / DND | assertions, fullscreen, call; Focus unavailable | DE-specific (GNOME `show-banners`), mostly unavailable | `SHQueryUserNotificationState`: `QUNS_BUSY` (fullscreen), `QUNS_RUNNING_D3D_FULL_SCREEN`, `QUNS_PRESENTATION_MODE`, `QUNS_QUIET_TIME`, `QUNS_NOT_PRESENT` [primary MS doc] — best of the three platforms |
| Per-process | proc_pidinfo/proc_pid_rusage (same uid) | `/proc/<pid>/stat`, `statm`, `io` — 3.7 µs/process [tested] | `NtQuerySystemInformation(SystemProcessInformation)` one call for all, or ToolHelp + `GetProcessTimes` with `PROCESS_QUERY_LIMITED_INFORMATION` [secondary] |
| Delegated-job accounting / limits | process group + `proc_pid_rusage`; `taskpolicy -b` | cgroup v2 scope (`systemd-run --user --scope -p CPUQuota=…`), `cpu.stat`, per-cgroup `cpu.pressure` | **Job object**: `JOBOBJECT_CPU_RATE_CONTROL_INFORMATION` (hard cap / weight) + `JOBOBJECT_BASIC_ACCOUNTING_INFORMATION` [primary MS doc] |
| Low-priority sensor thread | QoS BACKGROUND | `SCHED_IDLE`/nice 19, `PR_SET_TIMERSLACK` | `THREAD_PRIORITY_IDLE`, EcoQoS via `SetThreadInformation` [secondary] |
| Event tracing (avoid) | — | — | ETW: powerful but heavy; don't use in the always-on path |

## Linux PSI notes (tested on kernel 6.18 in this sandbox)

- Trigger format `"<some|full> <stall_us> <window_us>"`; windows 500 ms–10 s; events
  rate-limited to **one per window** [primary psi.rst; tested: 3 events in 6 s with 2 s
  window].
- **Unprivileged** triggers need window a multiple of 2 s [primary]. "Privileged"
  means **CAP_SYS_RESOURCE**, not uid 0: in this sandbox uid 0 *without* CAP_SYS_RESOURCE
  got `EINVAL` for a 1 s window, and `nobody` succeeded with 2 s [tested].
- CPU `full` at system level is meaningless (reported as 0 since 5.13) [primary].
- PSI may be compiled out or disabled (`psi=0` boot arg) on some distros → capability
  `Unavailable(NotSupported)`, fall back to `/proc/stat` [secondary].
- Per-cgroup `cpu.pressure`/`memory.pressure` exist in cgroup v2 → measure the job's own
  stall and the rest separately.

## Porting cost (relative, macOS adapter = 1.0) [estimate]

| Platform | Cost | Why |
|---|---|---|
| Linux | ~0.4 | text files + PSI triggers (better signal than macOS), cgroups give clean job accounting; desktop user signals are weak/variable. |
| Windows | ~0.8 | session-0 split (service + per-user agent for idle/QUNS), power notifications need a window or service control handler, no thermal, PDH/COM plumbing; Job objects are excellent for delegated work. |

Recommendation: implement Linux second (cheap, testable in CI, validates the port
shape), Windows third.
