# Hexagonal port: execution priority / power policy (+ Windows and Linux adapters)

Goal: the core expresses *intent* ("this work is deferrable bulk", "this is the control plane"), reads a
`PowerState`, and asks for resource samples. Adapters translate to QoS/EcoQoS/nice/cgroups. Portability cost
stays one adapter crate per OS; the core never names an OS API.

## 1. Port (sketch — names are ours, not from any crate)

```rust
// crate: fogar-core (no OS deps, no tokio types in the trait surface)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkClass {
    /// Network I/O, protocol state machines, timers. Must stay responsive; energy-efficient.
    Control,
    /// Short user-visible work with a deadline (rare). Temporary elevation.
    Urgent,
    /// Deferrable CPU/disk work: compaction, indexing, uploads that can wait. Efficiency cores, throttled I/O.
    Bulk,
}

pub trait ExecutionPolicy: Send + Sync + 'static {
    /// Called once at startup (process-wide baseline: e.g. Windows ignore-timer-resolution, Linux timer slack).
    fn apply_process_baseline(&self) -> Result<(), PolicyError>;
    /// Apply to the *calling* thread. Must be idempotent and cheap; may be called at every job start.
    fn apply_current_thread(&self, class: WorkClass) -> Result<(), PolicyError>;
    /// What the adapter actually achieved (for logs/metrics and the acceptance gate).
    fn describe(&self) -> PolicyReport;
}

pub trait PowerSignals: Send + Sync + 'static {
    /// Current snapshot (cheap). Adapters push updates into a channel owned by the app layer.
    fn snapshot(&self) -> PowerState;            // PowerState: see signals-and-load-shedding.md
    fn subscribe(&self, sink: Box<dyn Fn(PowerState) + Send + Sync>) -> Result<Subscription, PolicyError>;
}

pub trait ResourceMeter: Send + Sync + 'static {
    fn sample_self(&self) -> ResourceSample;
}
pub struct ResourceSample {
    pub cpu_user_ns: u64, pub cpu_sys_ns: u64,
    pub footprint_bytes: u64,               // macOS phys_footprint / Windows private commit or WS / Linux PSS or RSS
    pub idle_wakeups: Option<u64>,          // macOS ri_pkg_idle_wkups; others: None or ctx switches
    pub interrupt_wakeups: Option<u64>,
    pub energy_nj: Option<u64>,             // macOS ri_energy_nj (rusage v6) where available
    pub perf_core_cpu_ns: Option<u64>,      // macOS ri_user_ptime + ri_system_ptime (v6)
    pub threads: u32,
}
```
Wiring (app layer, not core): tokio `Builder::on_thread_start(move || policy.apply_current_thread(WorkClass::Control))`;
a dedicated Bulk executor thread calls `apply_current_thread(WorkClass::Bulk)` once; `Urgent` is applied and then
restored around a scope (RAII guard in the app layer). Load shedding is a pure core function of `PowerState`.

Test double: `FakePolicy` records calls; `FakeSignals` lets tests drive thermal/memory transitions.

## 2. Adapter mapping

| Concept | macOS (adapter-macos) | Windows (adapter-windows) | Linux (adapter-linux) |
|---|---|---|---|
| Control thread | `pthread_set_qos_class_self_np(QOS_CLASS_UTILITY, 0)` | leave default, or explicitly HighQoS: `SetThreadInformation(ThreadPowerThrottling)` with ControlMask=`THREAD_POWER_THROTTLING_EXECUTION_SPEED`, StateMask=0 | default policy, nice 0 |
| Bulk thread | `QOS_CLASS_BACKGROUND` (E-cores only, I/O tier 2, bg sockets) | EcoQoS: `THREAD_POWER_THROTTLING_EXECUTION_SPEED` on; `ThreadMemoryPriority` low; optionally `SetThreadPriority(THREAD_MODE_BACKGROUND_BEGIN)` [unverified constant availability per thread] | `SCHED_IDLE` (`sched_setscheduler`) or nice 19 + `ioprio_set` idle class; put in a cgroup with low `cpu.weight` / `cpu.idle=1` if the service manager provides one |
| Urgent (scoped) | `QOS_CLASS_USER_INITIATED`, restore after | HighQoS (throttling off) | nice 0 (cannot raise above 0 unprivileged) |
| Process baseline | launchd `ProcessType` (+ `LowPriorityIO`) in the plist — packaging, not code | `SetProcessInformation(ProcessPowerThrottling, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION)`; `ProcessMemoryPriority` = low for the bulk-heavy process; `PROCESS_MODE_BACKGROUND_BEGIN` only for pure-background processes | systemd unit: `CPUWeight=`, `MemoryHigh=`, `Nice=`, `TimerSlackNSec=`, `IOSchedulingClass=idle` for bulk; or `prctl(PR_SET_TIMERSLACK, ns)` per thread |
| Timer coalescing | GCD timer leeway / kevent `NOTE_LEEWAY`; tokio timers: align in app | `PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION`; never call `timeBeginPeriod` | `PR_SET_TIMERSLACK` (default 50 µs; "may be up to the specified number of nanoseconds late"); larger slack for Bulk threads |
| Thermal signal | `kOSThermalNotificationPressureLevelName` (notify fd) / `NSProcessInfo.thermalState` | no general per-app thermal API found; use effective power mode as proxy [unverified gap] | `/sys/class/thermal` polling is a timer → avoid; rely on PSI cpu stalls as proxy [design choice, unverified] |
| Power mode / battery | `kIOPSNotifyPowerSource`, `isLowPowerModeEnabled` | `PowerRegisterForEffectivePowerModeNotifications` (Win10 1809+; V2 1903+ tracks slider, battery saver, game mode); callback fires immediately with current value | UPower over D-Bus or `/sys/class/power_supply` uevents [unverified] |
| Memory pressure | `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE` | `CreateMemoryResourceNotification(LowMemoryResourceNotification)` (waitable handle) | PSI trigger on `/proc/pressure/memory` (or the cgroup's `memory.pressure`): write `"some <stall_us> <window_us>"`, poll for `POLLPRI`; window 500 ms–10 s; **unprivileged users: window must be a multiple of 2 s** |
| Self metrics | `proc_pid_rusage` v4/v6, `task_info` | `GetProcessTimes`, `GetProcessMemoryInfo`, `QueryProcessCycleTime` [secondary] | `/proc/self/stat`, `/proc/self/status` (`voluntary_ctxt_switches`), `/proc/self/smaps_rollup` |

Sources: Microsoft `SetProcessInformation`/`SetThreadInformation` docs (MicrosoftDocs/sdk-api @ 656e965):
"When a process opts into enabling `PROCESS_POWER_THROTTLING_EXECUTION_SPEED`, the process will be classified as
EcoQoS ... reducing CPU frequency or using more power efficient cores"; "If an application does not explicitly
enable ..., the system will use its own heuristics"; IGNORE_TIMER_RESOLUTION: "any current timer resolution
requests made by the process will be ignored". Linux: `Documentation/accounting/psi.rst`,
`Documentation/admin-guide/cgroup-v2.rst` (`cpu.idle` = "cgroup analog of the per-task SCHED_IDLE",
`memory.high` throttle), `prctl(2)` PR_SET_TIMERSLACK, systemd.exec/systemd.resource-control. Rust bindings
for Windows (`windows`/`windows-sys`) and the exact feature flags: **verify** before use.

## 3. Rules for adapters

1. Never use APIs that opt a thread out of the OS QoS system (macOS `pthread_setschedparam`/`setpriority` on
   QoS threads; avoid the `thread-priority` crate on macOS).
2. Report, don't panic: if a policy call fails, log once, expose in `describe()`, continue at default.
3. No polling inside adapters. If an OS offers only polling for a signal, poll from the core heartbeat at the
   coarsest acceptable rate and document it as a measured cost.
4. Adapters are covered by the same acceptance gate (they must not add idle wakeups or threads beyond those
   declared — e.g. the GCD queue for the memory-pressure source).
