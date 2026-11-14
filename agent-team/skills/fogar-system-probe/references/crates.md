# Crate evaluation (versions checked on crates.io 2026-11-12; sources read, not executed)

| Crate | Version (date) | Use for | Verdict |
|---|---|---|---|
| `sysinfo` | 0.40.3 (2026-11-11), MSRV 1.95 | prototype, insight UI, cross-platform per-process table | Good for **insight**, not for the always-on sensor. See below. |
| `mach2` | 0.7.0 (2026-08-30) | Mach types, `mach_timebase_info`, `task_info`, `mach_host_self` | Use for types; **lacks `host_processor_info`/`host_statistics64`** → write externs [primary source]. |
| `libc` | 0.2.191 (2026-10-16) | sysctlbyname, proc_pidinfo, proc_pid_rusage, proc_listallpids, `xsw_usage`, `vm_statistics64`, `host_processor_info`, QoS (`pthread_set_qos_class_self_np`) | Primary FFI source; mach fns marked deprecated("use mach2"); **no `rusage_info_v5/v6`**, no notify(3) bindings. |
| `libproc` | 0.15.0 (2026-11-04) | convenience wrappers | Skip: **bindgen build-dependency** (needs libclang at build), `Result<_, String>` errors, rusage only up to V4 [primary Cargo.toml/source]. |
| `darwin-libproc` | 0.2.0 (2020) | — | Unmaintained; skip. |
| `heim` | 0.0.11 (2020) | — | Dead; skip. |
| `psutil` | 5.4.0 (2025-07) | — | Less maintained than sysinfo; skip. |
| `dispatch2` | 0.3.1 (2026-02-26) | memory-pressure dispatch source, timers with leeway, queues | Use. `DispatchSource::new` is `unsafe`; `_dispatch_source_type_memorypressure`, `dispatch_source_memorypressure_flags_t` bound [primary]. |
| `objc2` / `objc2-foundation` | 0.6.5 / 0.3.2 | `NSProcessInfo.thermalState`, `isLowPowerModeEnabled`, notification names | Use (small surface; enable only needed features). |
| `objc2-io-kit` | 0.3.2 | IOPS (`ps` feature), IOPM assertions/power (`pwr_mgt`), `kIOHIDIdleTimeKey`, IORegistry | Use; prefer over `io-kit-sys` 0.5.0. |
| `objc2-core-foundation` | 0.3.2 | CF types for the above | Use. |
| `objc2-core-graphics` | 0.3.2 | `CGEventSourceSecondsSinceLastEventType`, `CGWindowListCopyWindowInfo`, `CGSessionCopyCurrentDictionary` | Optional (user-busy signals); pulls WindowServer connection. |
| `core-foundation` (servo) | 0.10.1 (2025-05) | — | Works, but mixing with objc2-core-foundation types is friction; pick objc2 family. |
| `macmon` | 0.11.0 (2026-11-06) | reference for IOReport/SMC/CLPC | **Read, don't depend**: private APIs, macOS-27 breakage documented by its author. |
| `starship-battery` | 0.12.0 (2026-09-27) | battery details | Optional; uses mach2 + objc2-io-kit; IOPS directly is simpler for AC/pct. |
| `user-idle` | 0.6.0 (2023) | — | Skip: old `mach` crate, `IOMasterPort` (deprecated), copies **all** IOHIDSystem properties per call. Write 30 lines yourself. |
| `system-idle-time` | 1.0.4 | — | Not needed. |
| `procfs` | 0.18.0 (2025-08) | Linux /proc parsing incl. pressure | Fine for Linux insight; for the hot path a 20-line parser of `/proc/stat` and `/proc/pressure/*` is enough. |
| `windows` / `windows-sys` | 0.62.2 / 0.61.2 | all Win32 calls | Use `windows-sys` in the sensor (no COM wrappers needed). |
| `wmi` | 0.18.4 | WMI queries | Avoid in the always-on path (COM, slow); insight only. |
| `zbus` / `logind-zbus` | 6.2.1 / 5.3.2 | logind idle, UPower, power-profiles-daemon | Use zbus if Linux desktop signals are wanted. |

## sysinfo 0.40.3 on macOS — what it really does [primary source read]

- CPU: `host_processor_info(PROCESSOR_CPU_LOAD_INFO)`; global usage requires two
  refreshes ≥ `MINIMUM_CPU_UPDATE_INTERVAL` = **200 ms** apart (Linux/Windows also
  200 ms; BSD 100 ms).
- Memory: `host_statistics64(HOST_VM_INFO64)` → subject to the kernel rate limiter.
- Processes (`refresh_processes_specifics`): `proc_listallpids`, then per PID
  `PROC_PIDTBSDINFO`/`PROC_PIDTASKINFO` (or ALLINFO), `PROC_PIDTHREADINFO`, and depending
  on `ProcessRefreshKind`: `KERN_PROCARGS2` sysctl (cmd/environ: copies the target's
  arg+env area — the most expensive call), `proc_pidpath`, cwd/root vnode info,
  `proc_pid_rusage(RUSAGE_INFO_V2)` for disk I/O. `ProcessRefreshKind::nothing()` +
  `.with_cpu().with_memory()` and `UpdateKind::OnlyIfNotSet` for exe/cmd minimise it.
- `multithread` feature (rayon) is **not** in defaults in 0.40 — keep it off for a
  daemon (avoid a thread pool). Disable default features; enable only `system`.
- Per-process CPU% uses `pti_total_*` deltas against an interval derived from summed
  CPU ticks (`SystemTimeInfo::get_time_interval`), converted via `mach_timebase_info`.
- GPU: `IOAccelerator` `PerformanceStatistics` → `Device Utilization %`.
- Same-uid limitation applies: for other users' processes TASKINFO fails → sysinfo
  falls back to partial data.

Verdict: for a 24/7 sensor that only needs ~6 counters, **direct FFI is smaller,
cheaper and more predictable** (no HashMap of all processes, no allocation per tick);
use sysinfo (or your own scanner) behind the `ProcessInspector` port for the
Activity-Monitor-like view, invoked on demand.

## API-stability notes

- objc2 family is pre-1.0 (0.x) with periodic breaking releases; pin minor versions and
  wrap behind the adapter so churn stays local.
- sysinfo also 0.x with frequent breaking changes (0.3x → 0.40 in ~2 years) [secondary
  from version history] — another reason to keep it in the insight slice only.
- Public Apple C APIs used here (host_*, proc_*, sysctl, notify, IOPS, dispatch) are
  ABI-stable for decades; private ones (IOReport, CLPC, SMC, DND files) are not.
