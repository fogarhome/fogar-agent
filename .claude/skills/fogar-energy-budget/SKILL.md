---
name: fogar-energy-budget
description: Energy, CPU-wakeup and memory budget rules for the fogar-agent long-running Rust daemon on Apple Silicon (MacBook Air M2, fanless), with Windows/Linux equivalents. Use when writing or reviewing anything that affects idle cost or footprint - tokio runtime/Builder config, worker_threads, current_thread vs multi_thread, spawn_blocking, timers, interval, sleep, timeout, MissedTickBehavior, heartbeats, WebSocket ping/keepalive, polling, channels/bounded queues, thread QoS (pthread_set_qos_class_self_np, QOS_CLASS_UTILITY/BACKGROUND, P vs E cores, taskpolicy, launchd ProcessType, App Nap, timer coalescing/leeway), EcoQoS/power throttling, nice/SCHED_IDLE/cgroups/PSI/timer slack, allocators (system malloc, mimalloc, jemalloc), phys_footprint vs RSS, memory leaks/fragmentation, release profile (lto, codegen-units, panic, strip, PGO/BOLT), dependency bloat, thermal state / memory pressure / battery load shedding, the execution-priority/power-policy hexagonal port, and the per-slice efficiency acceptance gate (powermetrics, top idlew/power, proc_pid_rusage, Instruments/xctrace, samply, DHAT, 10-minute idle test, regression thresholds).
---

# fogar-energy-budget

Budget for an always-on daemon that must be invisible on a fanless M2: **working targets** <1 % CPU idle,
<50 MB `phys_footprint`, idle wakeups ≲0.5/s. Targets are to be **measured**, not assumed. macOS
behaviour in this skill comes from Apple OSS sources/headers and docs and is **untested on the M2**; Linux
tokio numbers are real Linux measurements. Evidence labels: [primary] [secondary] [unverified]; details and
URLs in `RESEARCH.md`.

## Hard rules

1. **Idle = no armed timer except one coarse, aligned heartbeat.** Waiting on sockets/channels/fds costs 0
   wakeups (Linux-measured); every `interval`/`sleep` loop costs ≥1 wakeup per period, and tokio's
   hierarchical wheel adds cascade wakeups (1 s timer ≈ 2 wakeups/tick, 30 s ≈ 3/tick on Linux).
2. **Align far deadlines to a shared grid** (or run them from one heartbeat task): 50 staggered 30 s timers =
   3.55 wakeups/s, 50 aligned = 0.10 wakeups/s (Linux).
3. **No polling.** Use events: sockets, `watch`, `Notify`, notify(3) fds, dispatch sources, PSI triggers.
4. **Set QoS explicitly** on every thread you own. Default (no call) = `QOS_CLASS_DEFAULT`, eligible for P cores.
   Control/network threads → `QOS_CLASS_UTILITY`; deferrable batch → `QOS_CLASS_BACKGROUND` on its own thread.
5. **Never put the socket-owning thread at BACKGROUND**: BG QoS ⇒ darwinbg ⇒ "darwinbg threads always create bg
   sockets" + I/O tier 2 (XNU `thread_policy.c`) [primary].
6. **Never call `pthread_setschedparam`/`setpriority` (or the `thread-priority` crate) on macOS QoS threads** —
   it permanently opts the thread out of QoS (`pthread/qos.h`) [primary].
7. **Bounded everything**: channels, caches (by bytes), blocking pool (`max_blocking_threads`), WebSocket
   buffers/message caps, reconnect backoff (capped, jittered).
8. **Measure `phys_footprint`, never RSS** (includes compressed memory; this is what top `mem` shows).
9. **Every slice passes the idle gate** (below) against a stored baseline; cost increases need a reviewed
   baseline update with a reason.
10. Core code is OS-agnostic: priority/power only via the `ExecutionPolicy`/`PowerSignals`/`ResourceMeter`
    ports. No OS calls, no polling inside adapters.

## Decision tables

**Runtime** (tokio 1.54.1)
| Need | Choose |
|---|---|
| Lowest memory/threads, all async, no risk of blocking code | `new_current_thread()` + dedicated BG `std::thread` for batch |
| Tolerate an occasionally slow task | `new_multi_thread().worker_threads(2)` — never the default (8 on M2) |
| QoS on runtime threads | `.on_thread_start(|| set QoS UTILITY)` (runs on workers *and* blocking threads) |
| Blocking pool | `.max_blocking_threads(4..8)`; default 512, keep-alive 10 s |
| `event_interval`, `global_queue_interval`, `disable_lifo_slot` (unstable) | leave defaults — fairness/throughput knobs, not idle-cost knobs |
| Periodic tick | `interval` + `MissedTickBehavior::Delay` (default `Burst` replays missed ticks after sleep) |
| Wakeup telemetry | `.on_thread_unpark(counter++)` exported as unparks/s |

**Timers & keepalive**
| Situation | Do |
|---|---|
| Several periodic jobs | one heartbeat task (e.g. 30 s) dispatching all of them |
| Per-connection idle timeout | single timer pushed forward on traffic; fires only on real silence |
| WebSocket keepalive | answer server pings (tungstenite auto-pongs); else ping only after ≥30–60 s of silence; never fixed-rate; TCP keepalive for dead-peer detection |
| Need OS-coalesced timer | GCD `dispatch_source_set_timer` with leeway ≥10 % of interval (Apple guidance); no `DISPATCH_TIMER_STRICT` |
| Debounce bursts | arm one `sleep` only while events are pending |

tokio timers on macOS park in `kevent(timeout)`; XNU gives those kernel-urgency waits ≤1 ms slop
regardless of thread QoS (source reading, untested) — so coalescing must happen in our code.

**QoS map** (validate placement on M2 via `ri_cpu_time_qos_*` / `--show-process-qos`)
| Thread | QoS | Effect |
|---|---|---|
| tokio workers, blocking pool, network | UTILITY | energy-efficient; Edge/CLPC may still use P cores |
| batch CPU/disk (compaction, indexing) | BACKGROUND | E cores only ("never recommend execution on the P-cores", XNU Edge), throttled I/O |
| short user-visible deadline | USER_INITIATED, scoped, restore | — |
| process role | launchd `ProcessType` (Background/Standard/Adaptive) — **measure** latency+energy before choosing; Background may make sockets background [unverified] |

**Allocator**
| Situation | Choose |
|---|---|
| default | system malloc (xzone + kernel "deferred reclaim", no background thread) |
| measured alloc-heavy hot path or fragmentation | mimalloc v3 (crate default) + `mi_collect(true)` on idle/pressure; note no purge thread on macOS |
| jemalloc | avoid on macOS: background purge thread unavailable on Mach-O, `MADV_FREE` lazy accounting [primary configure.ac] |

**Release profile**: `lto="fat"`, `codegen-units=1`, `strip="symbols"` + archived dSYM
(`split-debuginfo="packed"`), `debug="line-tables-only"`, `panic="abort"` only if a crash-restart is cheap
(it kills the daemon on any task panic). PGO optional for CPU-bound slices; BOLT not applicable to Mach-O
[secondary]. Feature-minimal deps; reject crates with hidden threads/periodic timers. See `references/build-profile.md`.

## Load shedding (pure core function of `PowerState`)

Signals (event-driven): thermal = notify(3) `kOSThermalNotificationPressureLevelName` or
`NSProcessInfo.thermalState`; memory = `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE`; power = notify
`com.apple.system.powersources.source`, `isLowPowerModeEnabled`.
| State | Effect |
|---|---|
| Battery / Low Power Mode | defer batch, stretch keepalive/heartbeat where protocol allows, no prefetch |
| Thermal elevated | pause batch, CPU concurrency 1, longer debounce |
| Thermal serious/critical | protocol duties only, reject heavy jobs with typed `Overloaded` |
| Memory WARN/CRITICAL | shrink caps for **new** cache entries, release allocator memory once; do **not** walk and drop existing caches (Apple `source.h`) |
| Recovery | hysteresis ≥60 s |

## Port (hexagonal)

`ExecutionPolicy { apply_process_baseline(); apply_current_thread(WorkClass::{Control,Urgent,Bulk}); describe() }`,
`PowerSignals { snapshot(); subscribe() }`, `ResourceMeter { sample_self() -> cpu ns, footprint, idle/intr
wakeups, energy_nj?, p-core ns?, threads }`. Full sketch + adapters: `references/power-policy-port.md`.

| | macOS | Windows | Linux |
|---|---|---|---|
| Bulk | QoS BACKGROUND | EcoQoS: `THREAD_POWER_THROTTLING_EXECUTION_SPEED` (SetThreadInformation), low memory priority | `SCHED_IDLE` / nice 19, ioprio idle, cgroup `cpu.weight`/`cpu.idle` |
| Control | QoS UTILITY | default/HighQoS | default |
| Timers | app alignment; GCD leeway | `PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION`; never `timeBeginPeriod` | `PR_SET_TIMERSLACK` / `TimerSlackNSec=` |
| Memory signal | dispatch memorypressure | `CreateMemoryResourceNotification` | PSI trigger (unprivileged: window multiple of 2 s) |
| Power signal | IOPS notify, Low Power Mode | `PowerRegisterForEffectivePowerModeNotifications` | UPower/sysfs [unverified] |

## Pitfalls

* Default `worker_threads` = 8 on M2; default blocking pool 512 with no backpressure.
* `MissedTickBehavior::Burst` after system sleep → catch-up storm.
* Unbounded `mpsc`; `broadcast` without `Lagged` handling; caches capped by count, not bytes.
* tungstenite default 128 KiB eager read buffer + 64 MiB message cap per connection.
* mimalloc/jemalloc keep freed pages until the next allocator call — an idle daemon keeps its peak footprint.
* `Mutex` shared between BG and UTILITY threads → priority inversion on a busy E cluster; use channels.
* Logging/metrics crates with flush/export intervals; file watchers that poll.
* Holding power assertions / `NSActivityIdleSystemSleepDisabled` while idle.
* Trusting Activity Monitor "Energy" or `%CPU` alone: top's POWER taxes each idle wakeup as 500 µs CPU
  (`top/power.c`), i.e. 10 wakeups/s ≈ 0.5 % CPU.
* Instruments Power Profiler is iOS/iPadOS-only; on Mac use powermetrics, System Trace, Time Profiler, samply.

## Acceptance gate (per slice, on the M2)

- [ ] Release build, AC power, Low Power Mode off, env recorded (`sw_vers`, `pmset -g batt/therm`).
- [ ] Warm-up 120 s, then 600 s hands-off, sample every 30 s; 3 runs, median reported.
- [ ] Idle CPU ≤1.0 % (one core) and not >1.25× baseline (+0.05 floor).
- [ ] Pkg-idle wakeups ≤2.0/s hard, design target ≤0.5/s; not >1.25× baseline (+0.2 floor).
- [ ] `phys_footprint` end ≤50 MB; 10-min slope ≤2 MB/h; burst-then-idle returns within +10 MB after 600 s.
- [ ] Threads ≤ declared budget; every timer/thread the slice adds is declared (name, period, QoS).
- [ ] No power assertions held (`pmset -g assertions`).
- [ ] Memory-pressure handler verified with `memory_pressure -S -l warn`.
- [ ] Per milestone: 24 h soak, idle floor growth ≤5 MB.
Reference script (manual, not auto-run): `references/idle-gate.sh`. Protocol, tools, rusage v6 helper:
`references/measurement-and-gate.md`.

## References

- `references/qos-and-scheduling-macos.md` — QoS, Edge scheduler, darwinbg, timer leeway tables, ProcessType, taskpolicy
- `references/tokio-low-idle.md` — tokio source facts, Linux measurements, timers, channels, WebSocket
- `references/linux-tokio-idle-experiment.md` — reproducible experiment code + raw results
- `references/memory-and-allocators.md` — footprint, xzone/mimalloc/jemalloc, A/B protocol, fragmentation
- `references/build-profile.md` — profile, PGO/BOLT, size, startup, dependency rules
- `references/signals-and-load-shedding.md` — thermal/memory/power sources and shed policy
- `references/power-policy-port.md` — port definition, macOS/Windows/Linux adapters
- `references/measurement-and-gate.md` — tools, idle protocol, thresholds, triage
- `RESEARCH.md` — sources, confidence, conflicts, open questions, M2 measurement list
