# macOS probe API catalogue (Apple Silicon first)

Status: everything here is UNTESTED on macOS (research done from Linux). Labels:
[primary] = read in Apple open source (XNU xnu-12377.1.9, Libc-1752.100.10,
IOKitUser-100231.100.18.0.1, Libnotify-348.100.7) or crate source;
[secondary] = third-party source/forum/docs; [unverified] = belief, must be measured.

Assumed deployment: per-user **LaunchAgent in the Aqua session**, no root, not
sandboxed, hardened runtime. Everything below is chosen to work there.

## 1. System-wide CPU

### host_processor_info(PROCESSOR_CPU_LOAD_INFO) — per-CPU ticks
- Returns `processor_cpu_load_info { unsigned int cpu_ticks[CPU_STATE_MAX] }` per
  CPU; indices `CPU_STATE_USER=0, SYSTEM=1, IDLE=2, NICE=3` [primary osfmk/mach/machine.h].
- Ticks are **32-bit** and wrap: use `wrapping_sub` on deltas [primary type; macmon's
  test covers wrap, secondary].
- Out-array is vm-allocated by the kernel: **must `vm_deallocate(mach_task_self(), ptr,
  count*4)`** after every call or you leak (macmon and sysinfo both free it) [primary
  MIG semantics / crate source].
- **Not rate-limited** (the limiter in `osfmk/kern/host.c` only wraps
  `host_statistics`/`host_statistics64`) [primary].
- Order of `cpuN` vs P/E clusters is **chip dependent**; macmon explicitly refuses to
  map ticks to tiers ("cpuN order differs by chip (M6: E, S, P)") [secondary]. Get
  cluster sizes from `hw.nperflevels`, `hw.perflevelN.physicalcpu`, `hw.perflevelN.name`
  (perflevel0 = highest capability) [primary bsd/kern/kern_mib.c]. On M2: 4P+4E
  [secondary]; whether cpu0..3 are E is [unverified] — measure.
- Bindings: `libc::host_processor_info` exists but `mach_host_self` etc. are
  `#[deprecated(note="Use the mach2 crate")]` in libc 0.2.191; **mach2 0.7.0 does not
  bind `host_processor_info`/`host_statistics64`** (mach2 issue #34 referenced by
  macmon) [primary crate source]. → Declare the 2–3 externs yourself or
  `#[allow(deprecated)]` libc.
- Call `mach_host_self()` once and cache the port (each call returns a send right)
  [secondary, common Mach guidance].

### host_statistics(HOST_CPU_LOAD_INFO) / host_statistics64(HOST_VM_INFO64)
- **Rate limited for non-platform (third-party) binaries**: per flavor, a 1 s window,
  a random budget of 2..10 requests, then a **cached copy** is returned
  (`HOST_STATISTICS_TIME_WINDOW 1`, `MAX_REQUESTS 10`, `MIN_REQUESTS 2`) [primary
  osfmk/kern/host.c]. The cache is global per flavor, so other third-party tools
  consume the same budget. Rule: call ≤ 1×/s per flavor and tolerate a repeated
  (stale) value; detect it (identical counters) and do not treat as "0 activity".
- `vm_statistics64` gives pageins/pageouts, `compressions`, `decompressions`,
  `swapins`, `swapouts`, `compressor_page_count`, `free_count`, `internal_page_count`,
  `external_page_count`, `speculative_count`... [primary osfmk/mach/vm_statistics.h].
  Use **deltas of swapouts/compressions** as a thrash signal.

### getloadavg(3)
- Cheap, but load average on macOS is a scheduler-run-queue average with ~1/5/15 min
  decay; too slow alone. Use only as a secondary signal [secondary].

## 2. Memory

| Source | What | Notes |
|---|---|---|
| `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE` | event: NORMAL=0x1, WARN=0x2, CRITICAL=0x4 | public libdispatch API; bound in `dispatch2` 0.3.1 (`dispatch_source_memorypressure_flags_t`, `DispatchSource::new`) [primary crate]. Needs a GCD queue, not a run loop. |
| sysctl `kern.memorystatus_vm_pressure_level` | current level as the **same dispatch values** (1/2/4) | readable on macOS without privilege (priv check is `!XNU_TARGET_OS_OSX` only); `CTLFLAG_MASKED` so it is hidden from `sysctl -a` but readable by name [primary bsd/kern/kern_memorystatus_notify.c]. Use it to seed the state at startup and after wake. Internal Warning+Urgent → WARN. |
| sysctl `kern.memorystatus_level` | % available memory = `available_memory*100/total_pages` | [primary osfmk/vm/vm_pageout.c]. Good continuous complement. |
| sysctl `vm.swapusage` | `struct xsw_usage {xsu_total, xsu_avail, xsu_used, xsu_pagesize, xsu_encrypted}` | [primary bsd/sys/sysctl.h, kern_sysctl.c]. Swap *use* is weak; swap *rate* (vm_statistics64 deltas) is the signal. |
| `EVFILT_MEMORYSTATUS` (kqueue) | same levels | **private** (`event_private.h`) — don't use [primary]. |

## 3. Per-process (insight module; on demand, not in the steady loop)

- `proc_listallpids` / `proc_listpids(PROC_ALL_PIDS,…)`: no privilege [primary
  proc_info.c: LISTPIDS uses NO_CHECK_SAME_USER].
- `proc_pidinfo` flavors **without** same-user check: `PROC_PIDT_SHORTBSDINFO` (pid,
  ppid, uid, 16-char `pbsi_comm`), `PROC_PIDUNIQIDENTIFIERINFO`, `PROC_PIDPATHINFO`
  (`proc_pidpath`), `PROC_PIDCOALITIONINFO`, `PROC_PIDPLATFORMINFO` [primary].
- **Everything else** (`PROC_PIDTASKINFO`, `PROC_PIDTBSDINFO`, `PROC_PIDTHREADINFO`,
  and `proc_pid_rusage`) requires **same uid or root/PRIV_GLOBAL_PROC_INFO** → EPERM
  for other users' processes (root daemons, `_windowserver`, `_spotlight`…) [primary
  `proc_security_policy`]. Consequence: a non-root agent sees CPU/mem/energy of the
  user's own apps only; show the remainder as "system / other users" =
  total − Σ(visible). Note: libproc's `proc_name()` uses `PROC_PIDTBSDINFO` → fails
  for other users; use SHORTBSDINFO or `proc_pidpath` instead [primary libproc.c].
- `proc_taskinfo.pti_total_user/system` are **Mach absolute time units**
  (`recount_times_mach`) [primary osfmk/kern/bsd_kern.c] → convert with
  `mach_timebase_info` (on Apple Silicon 125/3, i.e. 24 MHz ticks [secondary]). Same
  for `ri_user_time`, `ri_system_time`, `ri_user_ptime` [primary task.c].
- `proc_pid_rusage(pid, RUSAGE_INFO_V6, …)` — `rusage_info_v6` adds
  `ri_energy_nj`, `ri_penergy_nj` (nanojoules, total and P-cores), `ri_user_ptime`,
  `ri_system_ptime` (time on P-cores), `ri_pinstructions`, `ri_pcycles`,
  `ri_neural_footprint`; v4+ has `ri_billed_energy`, `ri_serviced_energy`,
  `ri_instructions`, `ri_cycles`, `ri_runnable_time`; all versions have
  `ri_pkg_idle_wkups`, `ri_interrupt_wkups`, `ri_phys_footprint`,
  `ri_diskio_bytesread/written` [primary bsd/sys/resource.h, filled in
  osfmk/kern/bsd_kern.c `fill_task_rusage`]. **libc 0.2.191 only defines up to
  `rusage_info_v4`** and **libproc 0.15.0 only up to `RUsageInfoV4`** [primary crate
  source] → hand-write `#[repr(C)] struct RusageInfoV6` copied field-by-field from
  resource.h (incl. `ri_reserved[9]`). Fall back V6→V4 on EINVAL (older macOS).
  Whether `ri_energy_nj` is non-zero on M2 is [unverified].
- `phys_footprint` (`ri_phys_footprint`) is the memory number Activity Monitor calls
  "Memory" [secondary]; RSS over-counts shared pages.
- Cost: per process 2–4 syscalls. Linux analogue measured here: 3.7 µs/process for
  `/proc/<pid>/stat` [tested Linux]. Expect a few ms for ~600 processes on macOS
  [unverified] → never in the 5 s loop; run on UI request, or ≤ every 30 s while
  state=ocupado to attribute the cause ("top offenders").

## 4. Thermal

| API | Values | Event? | Notes |
|---|---|---|---|
| `NSProcessInfo.thermalState` | Nominal 0, Fair 1, Serious 2, Critical 3 | `NSProcessInfoThermalStateDidChangeNotification` | bound in objc2-foundation 0.3.2 [primary crate]. NotificationCenter delivery thread/run-loop needs [unverified] → poll the property each tick as backup (cheap getter [unverified cost]). |
| notify(3) `kOSThermalNotificationPressureLevelName` + `notify_get_state` | macOS enum: Nominal 0, Moderate 1, Heavy 2, Trapping 3, Sleeping 4 | yes | [primary Libc OSThermalNotification.h, macOS 10.10+]. Use the exported symbol, don't hardcode the string. Works with `notify_register_dispatch` **or `notify_register_file_descriptor`** (fd → kqueue/tokio, no GCD) [primary Libnotify notify.h]. |
| `IOPMGetThermalWarningLevel` / key `com.apple.system.power.thermal_warning` | Normal/Danger/Crisis | notify | coarse, mostly Intel-era [secondary]. |
| `powermetrics --samplers thermal` | pressure level + more | — | **root**. Not for the agent. |
| SMC / IOHIDEventSystem temps | °C | — | private APIs (macmon) — avoid in prod; temperatures are not needed for classification. |

MacBook Air M2 is fanless: thermal pressure is the **primary guard** — sustained
delegated load will raise it; treat Moderate/Fair as "stop adding work".

## 5. Power, battery, Low Power Mode

- `IOPSCopyPowerSourcesInfo` → `IOPSCopyPowerSourcesList` → `IOPSGetPowerSourceDescription`;
  keys `"Power Source State"` (`"AC Power"`/`"Battery Power"`), `"Current Capacity"`,
  `"Max Capacity"`, `"Is Charging"`, `"Time to Empty"` [primary IOPSKeys.h]. Bound in
  objc2-io-kit 0.3.2 feature `ps` [primary].
- Events via notify(3): `com.apple.system.powersources.source` (AC↔battery),
  `…powersources.lowbattery`, `…powersources.timeremaining`, `com.apple.system.powersources`
  (any change) [primary objc2-io-kit generated constants]. `IOPSNotificationCreateRunLoopSource`
  needs a CFRunLoop — prefer notify.
- Low Power Mode: `NSProcessInfo.isLowPowerModeEnabled` +
  `NSProcessInfoPowerStateDidChangeNotification` [primary objc2-foundation]. IOKit's
  `kIOPMLowPowerModeKey "LowPowerMode"` is in **IOPMLibPrivate.h** (0 auto / 1 low /
  2 high) [primary] → private, don't use.

## 6. User presence / "don't disturb" signals

| Signal | API | Cost / permission | Confidence |
|---|---|---|---|
| HID idle time | IORegistry `IOHIDSystem` property `"HIDIdleTime"` (ns) — use `IORegistryEntryCreateCFProperty` for the single key and cache the `io_service_t` | IOKit = daemon-safe (TN2083) [secondary]; no TCC | high [secondary] |
| idle (alt) | `CGEventSourceSecondsSinceLastEventType(kCGEventSourceStateCombinedSessionState, kCGAnyInputEventType)` (objc2-core-graphics) | connects to WindowServer; CG is not daemon-safe (TN2083) | high in Aqua agent |
| Video/presentation/screen kept awake | `IOPMCopyAssertionsStatus` → `"PreventUserIdleDisplaySleep"` > 0; per-process via `IOPMCopyAssertionsByProcess` | no root (`pmset -g assertions` is user-runnable) [secondary] | medium: also taken by e.g. `caffeinate` |
| Full-screen app | `CGWindowListCopyWindowInfo` layer-0 window covering a display; owner PID is readable, window *titles* need Screen Recording TCC since 10.15 [secondary] | O(windows) — poll only when user active | medium [unverified] |
| Foreground app | `NSWorkspace.frontmostApplication` / `didActivateApplicationNotification` (AppKit, needs main run loop) or front window owner from CGWindowList | AppKit in agent | medium |
| Screen sharing / remote session | presence of `screensharingd` process (`PROC_PIDT_SHORTBSDINFO` name) [secondary heuristic] | no root | low-medium |
| Camera / mic in use | CoreMediaIO `kCMIODevicePropertyDeviceIsRunningSomewhere`, CoreAudio `kAudioDevicePropertyDeviceIsRunningSomewhere` [secondary; names unverified here] | property query, no TCC [unverified] | medium; strong "in a call" signal |
| Focus / DND | **No supported headless API.** `INFocusStatusCenter.isFocused` needs `NSFocusStatusUsageDescription` and reportedly the communication entitlement; legacy DND observers reported broken on macOS 27 [secondary: Apple dev forum 856012]. `~/Library/DoNotDisturb/DB/*.json` is undocumented and TCC-protected [secondary]. | — | **treat as unavailable**; offer an explicit user toggle in fogar-agent instead |
| Screen locked / display asleep | `CGSessionCopyCurrentDictionary` key `CGSSessionScreenIsLocked` [secondary, undocumented key] | CG | low-medium |

SSH sessions do not generate HID events, so remote work by the gateway won't make
the user look "active" [unverified — measure].

## 7. GPU and Apple Silicon P/E detail

- GPU utilisation without root: IORegistry `IOAccelerator` → `"PerformanceStatistics"`
  → `"Device Utilization %"` (sysinfo 0.40.3 `gpu.rs`) [primary crate source; Apple
  key undocumented → secondary].
- Per-cluster residency / frequency / power: only via **IOReport** (private
  `libIOReport.dylib`: `IOReportCopyAllChannels`, `IOReportCreateSubscription`,
  `IOReportCreateSamples(Delta)` — macmon 0.11.0) or `powermetrics` (root). **macOS 27
  broke rootless "Energy Model" CPU/ANE counters** (need private entitlement
  `com.apple.private.pmgr.nrg.reporting`; `sudo` does not help); macmon now relies on
  undocumented AppleCLPC IDs per chip [secondary: macmon readme/source]. → Do **not**
  put IOReport in the classifier path; optional "insight" extra at most.
- Cheaper P/E proxy: per-process `ri_user_ptime/ri_system_ptime` vs total time tells how
  much of a process ran on P cores [primary field semantics, unverified values].

## 8. What needs root / entitlement

| Needs root | Needs TCC/entitlement | Works as user agent |
|---|---|---|
| `powermetrics` (all samplers); other-uid `proc_pidinfo(TASKINFO)`/`proc_pid_rusage`; `task_for_pid` | Focus status (INFocusStatusCenter + entitlement); window titles (Screen Recording); `~/Library/DoNotDisturb` (Full Disk Access) [secondary]; IOReport energy on macOS 27 (private entitlement, not root) | host_processor_info, host_statistics64 (rate-limited), sysctl memorystatus/swap/hw, dispatch memory pressure, notify(3) thermal/power, IOPS, HIDIdleTime, IOPM assertions status, IOAccelerator stats, own-uid per-process info |

## 9. Minimal FFI sketch (untested)

```rust
// Declare only what you call; keep it in the macOS adapter crate.
use libc::{c_int, c_uint, mach_port_t, kern_return_t};
extern "C" {
    fn mach_host_self() -> mach_port_t;
    fn host_processor_info(host: mach_port_t, flavor: c_int, out_cpu_count: *mut c_uint,
        out_info: *mut *mut c_int, out_info_cnt: *mut c_uint) -> kern_return_t;
    fn vm_deallocate(task: mach_port_t, addr: usize, size: usize) -> kern_return_t;
    static mach_task_self_: mach_port_t;
    // libnotify
    fn notify_register_file_descriptor(name: *const libc::c_char, fd: *mut c_int,
        flags: c_int, token: *mut c_int) -> u32;
    fn notify_get_state(token: c_int, state: *mut u64) -> u32;
    static kOSThermalNotificationPressureLevelName: *const libc::c_char;
}
const PROCESSOR_CPU_LOAD_INFO: c_int = 2; // [primary] osfmk/mach/processor_info.h
const HOST_CPU_LOAD_INFO: c_int = 3;      // [primary] osfmk/mach/host_info.h
const HOST_VM_INFO64: c_int = 4;          // [primary] osfmk/mach/host_info.h
```
Verify every constant against the SDK header at build time (bindgen-free: a unit test
that compares `size_of` of each `#[repr(C)]` struct with values printed by a tiny C
program on the M2, run once by a human).
