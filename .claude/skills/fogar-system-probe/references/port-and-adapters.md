# Hexagonal port, capability negotiation, runtime loop

Design sketch (not compiled). Names are proposals for fogar-agent, not existing APIs.

## 1. Crate / slice layout

```text
fogar-core/monitor/          (no OS deps, no unsafe)
  model.rs        RawSample, Reading<T>, Capability, LoadState, Reason, LoadReport
  smoothing.rs    Ewma{tau}, RateEstimator, Dwell
  classifier.rs   Classifier::step(&RawSample, now) -> Option<Transition>
  policy.rs       Policy (thresholds, require_ac, user_idle_relax...) – serde config
  scheduler.rs    AdaptiveInterval (decides next sample time)
  ports.rs        SystemProbe, ProcessInspector, LoadReportSink, Clock, DelegatedJobs
fogar-adapter-macos/monitor/  (unsafe FFI lives only here)
fogar-adapter-linux/monitor/
fogar-adapter-windows/monitor/
```
Feature flag `system-monitor` on the binary; runtime config `monitor.enabled`. When off
the adapter is never constructed (no threads, no fds, no timers) → zero cost.
The gateway then has no report → it must treat the host as `desconocido`. Optionally
publish one `desconocido(MonitoringDisabled)` at startup so the UI can explain why.

## 2. Ports

```rust
pub enum Fidelity { Event, Poll { min_interval: Duration }, Unavailable(Why) }
pub enum Why { NotSupported, NeedsRoot, NeedsPermission(&'static str), PrivateApi, Disabled }

pub struct Capabilities {
    pub cpu_total: Fidelity,          // macOS: Poll(1s) host_processor_info
    pub cpu_per_core: Fidelity,
    pub cpu_cluster_split: Fidelity,  // macOS: Unavailable(PrivateApi) by default
    pub memory_pressure: Fidelity,    // macOS: Event (dispatch); Linux: Event (PSI trigger)
    pub swap_activity: Fidelity,
    pub thermal: Fidelity,            // macOS: Event (notify); Linux: Poll (sysfs); Win: Unavailable
    pub power: Fidelity,              // Event on all three
    pub low_power_mode: Fidelity,
    pub user_idle: Fidelity,
    pub user_busy_signals: Fidelity,  // fullscreen/presentation/call
    pub focus_dnd: Fidelity,          // macOS: Unavailable(NeedsPermission("Focus"))
    pub gpu: Fidelity,
    pub per_process: Fidelity,        // Poll; scope: SameUserOnly | All
    pub per_process_energy: Fidelity,
}

pub trait SystemProbe: Send {
    fn capabilities(&self) -> &Capabilities;          // negotiated once at start
    /// Cheap, non-blocking, bounded time. Reads only what `want` asks for.
    fn sample(&mut self, want: SampleMask) -> Result<RawSample, ProbeError>;
    /// Register event sources; adapter pushes ProbeEvent into the core's channel.
    fn subscribe(&mut self, tx: EventSender) -> Result<Subscription, ProbeError>;
}

pub trait ProcessInspector: Send {   // insight module, on demand
    fn top(&mut self, n: usize, by: SortKey) -> Result<Vec<ProcessUsage>, ProbeError>;
    fn usage_of(&mut self, pids: &[Pid]) -> Result<Vec<ProcessUsage>, ProbeError>;
}
pub trait DelegatedJobs: Send { fn cpu_time_total(&mut self) -> Duration; } // pgid / cgroup / Job
pub trait LoadReportSink: Send { fn publish(&mut self, r: &LoadReport) -> Result<(), SinkError>; }
pub trait Clock: Send { fn now_mono(&self) -> Instant; fn now_incl_sleep(&self) -> Duration; }
```
`RawSample` holds cumulative **counters** (ticks, swapouts...) not percentages: the core
computes deltas, so adapters stay dumb and tests can replay raw data.

Capability negotiation rules:
1. Adapter probes each capability at startup (one real call; record error kind).
2. Core computes `coverage` weights from the result (classifier.md §7).
3. Capabilities are published to the gateway once per session (`hello` message) so the
   operator sees e.g. "thermal: event, focus: unavailable(permission)".
4. Re-probe a failed capability with exponential backoff (1 min → 1 h), not every tick.

## 3. Runtime loop (single thread, low QoS)

```text
thread "monitor" at QoS BACKGROUND/UTILITY (macOS pthread_set_qos_class_self_np,
  bound in libc::…/libpthread/pthread_/qos.rs) ; Linux SCHED_IDLE or nice 19 ; Windows
  THREAD_PRIORITY_IDLE + EcoQoS (SetThreadInformation ThreadPowerThrottling) [secondary]
loop:
  wait on (event channel | deadline = scheduler.next())
  if event: apply to state (thermal/mem/power/user toggle) → classify → maybe publish
  if deadline: s = probe.sample(mask_for(state)); classify; scheduler.update(...)
```
No tokio needed for the sensor. If the rest of the agent is tokio, give the monitor its
own OS thread and send `LoadReport` over a channel. Event sources that need GCD
(dispatch memory pressure) run their handler on a GCD queue that only forwards a
message.

## 4. Adaptive sampling (core::scheduler)

```text
interval bounds: min 2 s, base 5 s, max 60 s
stable: if |fast - slow| < 0.05 and no transition for 5 min → interval *= 1.5 (≤ max)
change: if |Δfast| > 0.15 or candidate transition pending → interval = min (burst ×6)
user active (idle < 60 s) → cap at 10 s; on battery → floor 15 s; display asleep → 60 s
pending dwell timer → wake exactly at dwell expiry, not earlier
```
Always schedule with **leeway** (≥10% of interval) so the OS can coalesce timers:
macOS `dispatch_source_set_timer(…, leeway)` (dispatch2 `DispatchSource::set_timer`
has a `leeway` arg [primary crate]); Linux `timerfd` + `prctl(PR_SET_TIMERSLACK)`;
Windows `SetWaitableTimerEx(TolerableDelay)` / `SetCoalescableTimer` [secondary].

Mask by state: in `libre` sample cpu+vm counters; per-process only when entering
`ocupado` (once, to record "cause"), then ≤ every 30 s while `ocupado`.

## 5. Report (published to the gateway)

```json
{ "v": 1, "host": "mac-ana", "seq": 1842, "state": "ocupado",
  "since": "2026-11-12T10:21:04Z", "observed_at": "2026-11-12T10:23:10Z",
  "ttl_s": 120, "confidence": 0.86,
  "reason": [{"code":"CpuSustained","slow":0.62,"for_s":75},{"code":"OnBattery","pct":71}],
  "capacity": {"cores": 0, "mem_mb": 0},
  "retry_after_s": 180,
  "metrics": {"cpu_total":0.71,"cpu_foreign":0.62,"cpu_delegated":0.09,
              "mem_level":"warn","thermal":"nominal","power":"battery"},
  "caps_hash": "9f3c…" }
```
Publish on every state transition and as a heartbeat (every `ttl_s/2`, e.g. 60 s).
`ttl_s` grows with the sampling interval but is never > 300 s.
