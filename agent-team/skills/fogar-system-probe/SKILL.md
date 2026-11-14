---
name: fogar-system-probe
description: Design and implement fogar-agent's optional system-monitoring module in Rust — cheap system probing (CPU ticks, memory pressure, swap, thermal, battery/AC, Low Power Mode, user idle/fullscreen/call signals, per-process CPU/memory/energy) and load classification into libre / ocupado / no_molestar / desconocido with smoothing, hysteresis, reason and confidence, published to a gateway that delegates SSH jobs. Use when touching macOS APIs like host_processor_info, host_statistics64, vm_statistics64, sysctl kern.memorystatus_*, vm.swapusage, hw.perflevel*, libproc proc_pidinfo/proc_pid_rusage (rusage_info_v6 energy), DISPATCH_SOURCE_TYPE_MEMORYPRESSURE, notify(3) thermal pressure, NSProcessInfo thermalState/isLowPowerModeEnabled, IOPowerSources, IOPM assertions, HIDIdleTime, CGEventSource, IOReport/powermetrics; when choosing crates (sysinfo, mach2, libc, libproc, objc2, dispatch2, objc2-io-kit, macmon); when designing the SystemProbe port, capability negotiation, adaptive sampling, the lease/backoff protocol, or the Linux (/proc, PSI, cgroups, sysfs) and Windows (GetSystemTimes, PDH, power notifications, SHQueryUserNotificationState, Job objects) adapters; or when measuring sensor CPU/energy/wakeups on an Apple Silicon (M2) laptop.
---

# fogar-system-probe

Goal: a sensor that costs ~nothing (<0.1% CPU avg, <1 wake/s, <15 MB for this slice —
targets, **to be measured**) and a state the gateway can trust. Status: macOS parts are
**untested** (researched from source); Linux PSI facts were tested. Labels:
[P]=primary source read, [S]=secondary, [U]=unverified. Details live in `references/`.

## 1. Non-negotiables

1. Agent runs as a **per-user LaunchAgent (Aqua session), no root**. Everything in the
   classifier path must work there.
2. **Never publish `libre` from missing, stale, or low-confidence data.** Gateway treats
   absent/expired reports as `desconocido` = not schedulable.
3. Module off ⇒ adapter never constructed: no thread, no timer, no fd.
4. Steady loop reads ~6 kernel counters; per-process scans are **on demand** only.
5. Subtract the delegated jobs' own CPU before classifying (else feedback oscillation).
6. No private APIs (IOReport, SMC, CLPC, DND files) in the classifier path.

## 2. macOS API table (Apple Silicon)

| Signal | API | Root? | Event/Poll | Cost / pitfall |
|---|---|---|---|---|
| CPU total + per core | `host_processor_info(PROCESSOR_CPU_LOAD_INFO)` | no | poll | µs [U]; **must `vm_deallocate` result**; 32-bit ticks wrap; not rate-limited [P] |
| CPU/VM totals | `host_statistics64(HOST_VM_INFO64)`, `host_statistics(HOST_CPU_LOAD_INFO)` | no | poll | **rate-limited for 3rd-party binaries: 2–10 calls per 1 s window per flavor, then cached copy** [P host.c] |
| Swap/thrash | `vm_statistics64` Δ`swapouts`/`compressions`; sysctl `vm.swapusage` | no | poll | use rates, not totals |
| Memory pressure | `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE` (dispatch2) + seed with sysctl `kern.memorystatus_vm_pressure_level` (1/2/4) | no | **event** | sysctl hidden (MASKED) but readable on macOS [P] |
| Available mem % | sysctl `kern.memorystatus_level` | no | poll | = available·100/total [P] |
| Thermal | notify(3) `kOSThermalNotificationPressureLevelName` (Nominal/Moderate/Heavy/Trapping/Sleeping) or `NSProcessInfo.thermalState` (Nominal/Fair/Serious/Critical) | no | **event** | `notify_register_file_descriptor` works without GCD [P]; fanless M2 ⇒ main guard |
| AC/battery | `IOPSCopyPowerSourcesInfo` + notify `com.apple.system.powersources.source` | no | **event** | keys "Power Source State", "Current Capacity" [P] |
| Low Power Mode | `NSProcessInfo.isLowPowerModeEnabled` + `NSProcessInfoPowerStateDidChangeNotification` | no | event | IOKit `LowPowerMode` key is private [P] |
| User idle | IORegistry `IOHIDSystem` → `HIDIdleTime` (single-key copy) | no | poll | daemon-safe IOKit; CGEventSource needs WindowServer [S TN2083] |
| Video/presenting | `IOPMCopyAssertionsStatus` → `PreventUserIdleDisplaySleep` | no | poll | also set by `caffeinate` |
| Fullscreen/front app/call | CGWindowList bounds; NSWorkspace; Core Audio/CMIO "running somewhere" | no (titles need Screen Recording) | poll when user active | [S/U] |
| Focus/DND | INFocusStatusCenter (+entitlement) — reported broken on macOS 27 | TCC | — | **treat Unavailable; offer an explicit user toggle** [S] |
| Per-process CPU/mem | `proc_pidinfo(PROC_PIDTASKINFO)` | **other uids need root** | on demand | times in **Mach ticks** → `mach_timebase_info` [P] |
| Per-process energy | `proc_pid_rusage(RUSAGE_INFO_V6)`: `ri_energy_nj`, `ri_penergy_nj`, `ri_user_ptime`, wakeups, `ri_phys_footprint` | same rule | on demand | libc/libproc stop at v4 → hand-write struct [P] |
| Names of any pid | `PROC_PIDT_SHORTBSDINFO`, `proc_pidpath` | no | — | `proc_name()` fails for other uids [P] |
| GPU % | IOAccelerator `PerformanceStatistics["Device Utilization %"]` | no | poll | undocumented key [S] |
| P/E cluster residency, power | IOReport (private) / `powermetrics` | root/entitlement | — | **macOS 27 froze rootless Energy Model counters** [S macmon] — insight-only |
| P/E topology | sysctl `hw.nperflevels`, `hw.perflevelN.{physicalcpu,name}` | no | once | cpuN↔cluster order is chip-specific [S] |

More: `references/macos-apis.md`.

## 3. Event vs poll rules

- Event for discrete states: memory pressure, thermal, power source, LPM, user toggle.
  Seed each from a one-shot read at start and after wake.
- Poll only counters that have no event (CPU ticks, swap rates, idle time).
- Adaptive interval: base 5 s, min 2 s on change/pending transition, ×1.5 up to 60 s
  when stable; ≥15 s on battery; 60 s when display asleep. Always set **timer leeway
  ≥10%** (dispatch timer leeway / timerfd slack / coalescable timers).
- Run the monitor on one OS thread at **QoS BACKGROUND/UTILITY**
  (`pthread_set_qos_class_self_np`); no tokio needed inside the sensor.
- Wake from sleep (gap between `mach_continuous_time` and `mach_absolute_time`) ⇒ reset
  deltas, `desconocido(WarmingUp)` for ~3 samples.

## 4. Classifier (core, pure Rust) — defaults to calibrate

- Features: `cpu_foreign = cpu_total − cpu_delegated`; EWMA fast τ15 s, slow τ90 s
  (`alpha = 1−exp(−dt/τ)` so variable intervals stay correct).
- → `ocupado`: slow ≥0.55 for 60 s | fast ≥0.80 for 20 s | mem WARN 10 s | swapout
  ≥64 pg/s 30 s | thermal ≥Fair/Moderate | on battery (policy `require_ac`, default on) |
  user active (idle<60 s) & fast ≥0.50 for 10 s.
- → `no_molestar`: user toggle | LPM | thermal ≥Serious/Heavy | mem CRITICAL | battery
  <20% off AC | camera/mic in use | fullscreen/presenting while user active.
- → `libre` only when ALL clear for 120 s (slow ≤0.35, fast ≤0.50, mem normal, no
  swapping, thermal nominal, guards ok). `no_molestar` exits via `ocupado`.
- Min dwell 30 s; escalations immediate. Asymmetric = fail-busy.
- Confidence = freshness × coverage × warm-up × stability; <0.4 ⇒ `desconocido`.
- Reason = closed enum codes + numbers, decisive first.
Full spec: `references/classifier.md`.

## 5. Port (hexagonal)

```rust
pub trait SystemProbe: Send {
    fn capabilities(&self) -> &Capabilities;           // Fidelity per signal: Event | Poll{min} | Unavailable(Why)
    fn sample(&mut self, want: SampleMask) -> Result<RawSample, ProbeError>; // raw counters, no %
    fn subscribe(&mut self, tx: EventSender) -> Result<Subscription, ProbeError>;
}
pub trait ProcessInspector { fn top(&mut self, n: usize, by: SortKey) -> Result<Vec<ProcessUsage>, ProbeError>; }
pub trait DelegatedJobs   { fn cpu_time_total(&mut self) -> Duration; }
pub trait LoadReportSink  { fn publish(&mut self, r: &LoadReport) -> Result<(), SinkError>; }
pub trait Clock           { fn now_mono(&self) -> Instant; fn now_incl_sleep(&self) -> Duration; }
```
Core: deltas, EWMA, classifier, policy, scheduler, report/lease logic. Adapters: FFI
only, return raw counters + capability verdicts (`NeedsRoot`, `NeedsPermission`,
`PrivateApi`, `NotSupported`). Publish capabilities once per session; re-probe failed
ones with backoff. See `references/port-and-adapters.md`.

## 6. Gateway contract

Report `{state, since, observed_at, ttl_s, confidence, reason[], capacity, retry_after_s,
metrics}` on each transition + heartbeat every ttl/2. Gateway: stale/absent/low
confidence ⇒ `desconocido`. Jobs need a **lease** granted by the agent's live
classifier (≤5 min, renewed), revocable (ocupado: stop new work; no_molestar: pause).
Backoff: exp + full jitter, base 30 s, cap 30 min; no_molestar ≥15 min or push.
Run delegated jobs at background priority (`taskpolicy -b`). See
`references/gateway-protocol.md`.

## 7. Windows / Linux equivalents (`references/windows-linux.md`)

| | Linux | Windows |
|---|---|---|
| CPU | `/proc/stat` (5 µs/read, tested) + **PSI** `/proc/pressure/*` | `GetSystemTimes` (kernel time includes idle!) / PDH |
| Mem event | PSI trigger + `poll(POLLPRI)` (unprivileged: window multiple of 2 s; privilege = CAP_SYS_RESOURCE — tested) | `CreateMemoryResourceNotification` |
| Thermal | sysfs `thermal_zone*/temp` (poll) | none reliable → Unavailable |
| Power/LPM | sysfs power_supply, UPower, power-profiles-daemon | `GetSystemPowerStatus` (+SystemStatusFlag), `RegisterPowerSettingNotification`, `PowerRegisterForEffectivePowerModeNotifications` |
| User | logind IdleHint (DE-dependent) | `GetLastInputInfo` (session-specific → per-user process), `SHQueryUserNotificationState` (QUNS_BUSY/PRESENTATION/D3D) |
| Job accounting | cgroup v2 scope `cpu.stat`, `cpu.pressure` | Job object accounting + CPU rate control |
Cost vs macOS adapter: Linux ~0.4, Windows ~0.8 [estimate].

## 8. Crates (`references/crates.md`)

Sensor: `libc` 0.2.191 + own externs (mach2 0.7.0 lacks host_processor_info/
host_statistics64), `dispatch2` 0.3.1, `objc2-foundation`/`objc2-io-kit`/
`objc2-core-foundation` 0.3.2. Insight view: `sysinfo` 0.40.3 with default features off,
`multithread` off, minimal `ProcessRefreshKind` (KERN_PROCARGS2 is its costly path).
Skip `libproc` 0.15 (bindgen build-dep, rusage ≤v4), `user-idle`, `heim`. Read
`macmon` 0.11 for IOReport knowledge; don't depend on it.

## 9. Pitfalls

- host_statistics cache returns identical data → mark stale, don't feed EWMA.
- Leaking host_processor_info arrays; calling `mach_host_self()` repeatedly.
- Mach-tick vs ns confusion (Apple Silicon timebase ≠ 1).
- Wrong same-uid assumptions: other users' processes → EPERM; show "system/other".
- Per-process scans in the hot loop; KERN_PROCARGS2 per tick.
- Treating thermal as °C: use pressure/state, not temperatures.
- Run-loop-only APIs (IOPSNotificationCreateRunLoopSource, NSWorkspace notifications)
  in a thread without a run loop — prefer notify(3)/dispatch.
- Flapping from symmetric thresholds; forgetting the delegated job is the load.
- Assuming DND/Focus is detectable; assuming `libre` when the module is off.

## 10. Checklist

- [ ] Monitor compiled behind `system-monitor` feature; runtime off ⇒ zero threads.
- [ ] Capabilities probed, logged, sent in `hello`; Unavailable reasons explicit.
- [ ] Event sources: memory, thermal, power, LPM, user toggle; seeded at start/wake.
- [ ] CPU poll with adaptive interval + leeway; QoS background thread.
- [ ] Deltas wrap-safe; stale-cache and wake detection; ticks↔ns via timebase.
- [ ] Delegated CPU subtracted; guards use totals.
- [ ] Hysteresis, dwell, asymmetric exit; confidence gate; closed reason codes.
- [ ] Report TTL + heartbeat; gateway treats stale as desconocido; lease + revoke.
- [ ] Self-budget watchdog (own CPU, wakeups, footprint via proc_pid_rusage(self)).
- [ ] Replay tests from recorded M2 traces; measurement plan executed
      (`references/measurement-plan.md`); sources/open questions in `RESEARCH.md`.
