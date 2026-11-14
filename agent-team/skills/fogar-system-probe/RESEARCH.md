# RESEARCH — fogar-system-probe (2026-11-12)

Method: shallow/sparse clones and crate tarballs into fresh empty directories, read
only; no downloaded code executed. Only my own std-only Rust benchmark and a Python
PSI test were run, on Linux 6.18 (x86_64 VM, 2 vCPU). **No macOS testing was
possible: every macOS behaviour/cost claim is untested.**

## 1. Sources

Primary (read directly):
- XNU `apple-oss-distributions/xnu` tag/commit `xnu-12377.1.9` (2025-10-16; macOS 26-era):
  `osfmk/kern/host.c` (host_statistics rate limiter), `osfmk/kern/processor.c`,
  `osfmk/mach/{host_info.h,processor_info.h,machine.h,vm_statistics.h,mach_host.defs}`,
  `bsd/sys/{resource.h,proc_info.h,sysctl.h,event_private.h}`,
  `bsd/kern/{proc_info.c,kern_resource.c,kern_memorystatus.c,kern_memorystatus_notify.c,kern_mib.c,kern_sysctl.c}`,
  `osfmk/kern/{bsd_kern.c,task.c,recount.h}`, `osfmk/vm/vm_pageout.c`,
  `libsyscall/wrappers/libproc/{libproc.h,libproc.c}`.
- Apple Libc `Libc-1752.100.10`: `include/libkern/OSThermalNotification.h`.
- Apple IOKitUser `IOKitUser-100231.100.18.0.1`: `pwr_mgt.subproj/IOPMLib.h`,
  `IOPMLibPrivate.h`, `ps.subproj/IOPSKeys.h`.
- Apple Libnotify `Libnotify-348.100.7`: `notify.h`, `notify_keys.h`.
- Linux `Documentation/accounting/psi.rst` (torvalds/linux master, fetched 2026-11-12).
- Microsoft sdk-api docs (MicrosoftDocs/sdk-api markdown): QUERY_USER_NOTIFICATION_STATE,
  SYSTEM_POWER_STATUS, PowerRegisterForEffectivePowerModeNotifications,
  CreateMemoryResourceNotification, GetSystemTimes, GetLastInputInfo, GetProcessTimes,
  JOBOBJECT_CPU_RATE_CONTROL_INFORMATION.
- Crate sources (static.crates.io): sysinfo 0.40.3, mach2 0.7.0, libproc 0.15.0,
  libc 0.2.191, dispatch2 0.3.1, objc2-foundation 0.3.2, objc2-io-kit 0.3.2,
  objc2-core-graphics 0.3.2, objc2-app-kit 0.3.2, macmon 0.11.0, starship-battery
  0.12.0, user-idle 0.6.0, system-idle-time 1.0.4, procfs 0.18.0. Versions/dates from
  crates.io API on 2026-11-12.

Secondary:
- Apple TN2083 "Daemons and Agents" (daemon-safe frameworks table, GUI agents).
- Apple Developer Forums thread 856012 (INFocusStatusCenter on macOS 27; poster's
  report, no Apple answer).
- alexwlchan `get_focus_mode` script (reads `~/Library/DoNotDisturb/DB/*.json`).
- macmon 0.11.0 readme/comments (macOS 27 IOReport entitlement change, CLPC IDs,
  cpuN order differs by chip, `proc T:S:P:E` formats).

## 2. Claims and confidence

| # | Claim | Label | Confidence |
|---|---|---|---|
| 1 | host_statistics/host_statistics64 are rate-limited for non-platform binaries: 1 s window, random 2–10 requests, then cached per-flavor data | primary (host.c) | high |
| 2 | host_processor_info is not wrapped by the limiter | primary (host.c only wraps the two) | high |
| 3 | CPU tick arrays are 32-bit; CPU_STATE_USER/SYSTEM/IDLE/NICE = 0..3; PROCESSOR_CPU_LOAD_INFO=2, HOST_CPU_LOAD_INFO=3, HOST_VM_INFO64=4 | primary | high |
| 4 | proc_pidinfo flavors other than SHORTBSDINFO/UNIQID/PATH/COALITION/PLATFORM and proc_pid_rusage require same uid or PRIV_GLOBAL_PROC_INFO | primary (proc_info.c) | high (MACF/sandbox may further restrict) |
| 5 | pti_total_* and ri_user/system/ptime are Mach absolute time units | primary (`recount_times_mach`, `rm_time_mach`) | high |
| 6 | rusage_info_v6 is RUSAGE_INFO_CURRENT and has ri_energy_nj / ri_penergy_nj / ri_*ptime / ri_neural_footprint | primary (xnu-12377) | high for that XNU; availability per macOS release unverified |
| 7 | ri_energy_nj is non-zero/meaningful on M2 | — | unverified |
| 8 | kern.memorystatus_vm_pressure_level readable without privilege on macOS, returns dispatch levels 1/2/4; MASKED | primary | high |
| 9 | kern.memorystatus_level = available·100/total | primary (vm_pageout.c) | high |
| 10 | DISPATCH_SOURCE_TYPE_MEMORYPRESSURE flags 0x1/0x2/0x4 bound in dispatch2 0.3.1 | primary (crate) | high |
| 11 | OSThermalPressureLevel on macOS: Nominal..Sleeping 0..4; notify name symbol since 10.10 | primary (Libc header) | high |
| 12 | notify_register_file_descriptor exists (fd-based delivery) | primary (notify.h) | high |
| 13 | IOPS keys and notify names (powersources.source, lowbattery, timeremaining) | primary (IOKitUser, objc2-io-kit) | high |
| 14 | kIOPMLowPowerModeKey is private; use NSProcessInfo.isLowPowerModeEnabled | primary | high |
| 15 | IOKit/Foundation daemon-safe; ApplicationServices (CG) needs GUI session | secondary (TN2083, 10.5-era table) | medium |
| 16 | Focus/DND has no reliable headless API; broken legacy observers on macOS 27 | secondary (forum) | medium |
| 17 | macOS 27 froze rootless IOReport Energy Model counters (needs private entitlement) | secondary (macmon) | medium-high |
| 18 | cpuN order vs P/E cluster differs by chip | secondary (macmon) | medium |
| 19 | mach2 0.7.0 lacks host_processor_info/host_statistics64; libc marks mach fns deprecated; libc lacks rusage v5/v6 | primary (crate source) | high |
| 20 | libproc 0.15.0 has a bindgen build-dependency and rusage up to V4 | primary | high |
| 21 | sysinfo 0.40.3: MINIMUM_CPU_UPDATE_INTERVAL 200 ms on macOS; uses KERN_PROCARGS2 for cmd/environ; multithread not default; GPU via IOAccelerator | primary | high |
| 22 | Linux PSI: unprivileged triggers need 2 s-multiple windows; privilege is CAP_SYS_RESOURCE; one event per window | primary + **tested** | high |
| 23 | Linux read costs: /proc/stat 5.0 µs, /proc/pressure/* 2.4–2.5 µs, /proc/meminfo 3.5 µs, /proc/<pid>/stat ≈3.7 µs/process (69 pids, 259 µs) | **tested** (this VM) | medium (VM, small process count) |
| 24 | GetSystemTimes kernel time includes idle | primary (MS doc) | high |
| 25 | GetLastInputInfo is session-specific | primary (MS doc) | high |
| 26 | SHQueryUserNotificationState values incl. QUNS_BUSY, D3D full screen, presentation, quiet time | primary (MS doc) | high |
| 27 | PowerRegisterForEffectivePowerModeNotifications: Win10 1809+ desktop | primary (MS doc) | high |
| 28 | Windows thermal has no reliable public API | secondary | medium |
| 29 | `taskpolicy -b` keeps work on E cores / throttles I/O | secondary | medium — verify |
| 30 | `/bin/ps`, Activity Monitor see all processes via privilege (setuid / sysmond helper) | secondary | medium |
| 31 | All thresholds (0.35/0.55/0.80, τ15/90 s, dwell 30 s, exit 120 s) | design default | unverified — calibrate |
| 32 | Cost estimates for macOS calls (µs/ms) | — | unverified |

## 3. Conflicts / tensions found

- libc says "use the `mach2` crate" for mach functions, but mach2 0.7.0 does not bind
  `host_processor_info`/`host_statistics64` (macmon references mach2 issue #34). Resolution:
  own externs or `#[allow(deprecated)]` libc.
- Thermal enums differ: NSProcessInfo (Nominal/Fair/Serious/Critical, 4 values) vs
  OSThermalPressureLevel on macOS (Nominal/Moderate/Heavy/Trapping/Sleeping, 5 values;
  iOS uses different numeric values). Map both into one core enum; don't assume 1:1.
- macmon's readme says rootless CPU power "broke" on macOS 27, yet macmon still ships
  rootless numbers via undocumented CLPC IDs — usable for a hobby monitor, not for a
  sensor whose correctness we promise.
- TN2083 is old (10.5 table) but still the only Apple statement on daemon-safe
  frameworks; modern behaviour of CG from a LaunchDaemon is unverified.
- Linux doc says "unprivileged users" get the 2 s restriction; test showed uid 0 without
  CAP_SYS_RESOURCE is also "unprivileged" — consistent with kernel using a capability,
  but worth noting for containers.

## 4. Open questions

1. Real per-call costs on M2 (macOS 26/27) for each probe (measurement-plan.md §A).
2. Does `NSProcessInfoThermalStateDidChangeNotification` arrive in a process with no
   main run loop? (Fallback: notify fd + polling the getter.)
3. Is `rusage_info_v6` accepted on the user's macOS version; is `ri_energy_nj` populated
   for same-uid processes; does it track Activity Monitor's Energy Impact?
4. HIDIdleTime semantics with Screen Sharing / Universal Control / remote input.
5. Reliable "in a call" detection without TCC prompts (CoreAudio/CMIO property names to
   be verified against the SDK headers on the Mac).
6. cpuN↔perflevel mapping on M2; is per-cluster load needed at all for the classifier?
7. How quickly a fanless M2 Air reaches Fair/Serious under delegated load, and whether
   background-QoS jobs avoid it.
8. Whether App Sandbox (if fogar-agent is ever sandboxed/notarized for App Store) blocks
   any of these calls — sysinfo has an `apple-sandbox` feature that disables some.
9. Whether the user's org MDM restricts LaunchAgents or TCC grants.

## 5. What to measure on the M2

See `references/measurement-plan.md` (micro-costs, 72 h budget run, 12 behavioural
facts). Minimum before shipping: A (costs), B (72 h CPU/wakeups/footprint), C1, C2, C3,
C7, C10, C11.
