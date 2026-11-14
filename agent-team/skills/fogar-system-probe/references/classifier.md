# Load classifier: model, smoothing, hysteresis, confidence

All of this lives in **core** (pure Rust, no OS calls, fully unit-testable with a fake
clock and recorded traces). Thresholds are DEFAULTS to calibrate on the M2 (see
measurement-plan.md) — they are design choices, not measured facts [unverified].

## 1. States and their contract

| State | Meaning for the gateway | Typical causes |
|---|---|---|
| `libre` | may start new jobs up to `capacity` | sustained headroom, guards green |
| `ocupado` | do not start new jobs; running jobs may continue; retry after `retry_after` | sustained foreign CPU, memory WARN, thermal Fair/Moderate, on battery (default policy), user active + high load |
| `no_molestar` | do not start; ask running jobs to pause/checkpoint; long backoff | explicit user toggle, thermal Serious+, memory CRITICAL, LPM, battery low, call/presentation/fullscreen detected |
| `desconocido` | treat as NOT schedulable | module off, probe failure, stale data, warm-up after boot/wake, clock anomaly |

Ordering for precedence: `desconocido` (if data invalid) > `no_molestar` > `ocupado` > `libre`.

## 2. Inputs (features) per tick

```text
cpu_busy_total      0..1  = 1 - Δidle/Δall over host_processor_info ticks (all CPUs)
cpu_busy_foreign    0..1  = cpu_busy_total - cpu_delegated (see §6)
cpu_busy_max_core   0..1  hottest CPU (detects 1 saturated thread)
mem_level           Normal|Warn|Critical  (dispatch event, seeded by sysctl)
mem_avail_pct       0..100 kern.memorystatus_level
swapout_rate        pages/s  Δvm_statistics64.swapouts
compress_rate       pages/s  Δcompressions
thermal             Nominal|Fair/Moderate|Serious/Heavy|Critical/Trapping+
power               AC|Battery(pct, charging)|Unknown ; low_power_mode bool
user_idle_s         HIDIdleTime
user_signals        {display_sleep_prevented, fullscreen, camera_or_mic, screen_shared, user_dnd_toggle}
self_cpu, self_rss  own budget watchdog (proc_pid_rusage(getpid()))
```
Every input carries `(value, observed_at, source: Event|Poll, available: bool)`.

## 3. Smoothing

- Two EWMAs on `cpu_busy_foreign`, time-constant based so variable sampling is correct:
  `alpha = 1 - exp(-dt/tau)`; `fast` τ = 15 s, `slow` τ = 90 s.
- Do NOT smooth discrete event inputs (memory level, thermal, power): they are already
  kernel-debounced; apply only dwell/hysteresis.
- Rates (swapouts) → EWMA τ = 30 s.
- Discard the first delta after wake/resume or after a `Δall == 0` (no ticks) sample.

## 4. Rules (defaults)

```text
ENTER ocupado if any (sustained):
  slow_cpu  ≥ 0.55                          for ≥ 60 s
  fast_cpu  ≥ 0.80                          for ≥ 20 s
  mem_level == Warn                         for ≥ 10 s
  swapout_rate ≥ 64 pages/s (≈1 MiB/s@16K)  for ≥ 30 s
  thermal ≥ Fair/Moderate                   immediately
  power == Battery && policy.require_ac     immediately
  user_idle_s < 60 && fast_cpu ≥ 0.50       for ≥ 10 s   (user-activity override: protect the human)
ENTER no_molestar if any:
  user_dnd_toggle | low_power_mode | thermal ≥ Serious/Heavy | mem_level == Critical
  battery_pct < 20 && !AC | camera_or_mic | (fullscreen|display_sleep_prevented) && user_idle_s < 300
EXIT to libre only if ALL hold continuously for ≥ 120 s:
  slow_cpu ≤ 0.35, fast_cpu ≤ 0.50, mem_level == Normal, swapout_rate ≈ 0,
  thermal == Nominal, power guard ok, no no_molestar trigger
EXIT no_molestar → ocupado (never straight to libre) when its trigger clears for ≥ 60 s.
Min dwell in any state: 30 s, except escalations (→ no_molestar / desconocido) which are immediate.
```

Why: **asymmetric** — quick to protect the user, slow to declare `libre` ("fail
busy"). The gap 0.35/0.55 is the hysteresis band; the dwell kills flapping from bursty
foreground apps (compiles, browser tabs).

If the user is idle (`user_idle_s ≥ 300`) the policy may relax CPU entry thresholds
(e.g. slow 0.70) — but never thermal/memory/power guards.

## 5. Capacity offered when `libre`

`capacity_cores = floor( ncpu_perf * (1 - slow_cpu) * k_thermal * k_power - reserve )`
with `reserve = 1` core for the user, `k_thermal = 1.0 (Nominal)`, `k_power = 1.0 (AC)`.
On a fanless Air prefer advertising **E-core-friendly** work: the gateway should start
jobs with background QoS (`taskpolicy -b`/`-c background` on macOS) so the scheduler
keeps them off the P cores when the user needs them [secondary: taskpolicy(8); verify].

## 6. Subtracting our own delegated load (critical)

Delegated jobs make the machine busy by design. Without correction the state flips to
`ocupado` the moment a job starts, the gateway backs off, the job ends, state returns
to `libre`, job starts... a feedback oscillation.
- The agent tracks delegated job processes (process group / session id it spawned, or
  a dedicated user). macOS: sum Δ(`ri_user_time + ri_system_time`) of those PIDs
  (same uid ⇒ allowed) → `cpu_delegated`. Linux: cgroup v2 `cpu.stat usage_usec` of
  the job's scope. Windows: Job object `JOBOBJECT_BASIC_ACCOUNTING_INFORMATION`.
- Classify on `cpu_busy_foreign`; report both numbers.
- Thermal, memory and power guards use **total** state (the job heats the chip too).

## 7. Confidence (0..1) and reason

```text
confidence = Π factors, each in [0,1]:
  freshness     = 1 if newest cpu sample age ≤ 2·interval, decays to 0 at 5·interval
  coverage      = available_weighted_inputs / total_weights  (cpu .4, mem .2, thermal .2, power .1, user .1)
  warmup        = min(1, samples_since_start_or_wake / 6)
  stability     = 1 - min(1, transitions_last_10min / 6)
if confidence < 0.4 → state = desconocido (reason: low_confidence)
```
`reason` = ordered list of machine-readable codes + numbers, first = decisive:
`["cpu_sustained", {"slow":0.62,"for_s":75}], ["on_battery", {"pct":71}]`.
Codes are a closed enum (`CpuSustained, CpuBurst, UserActiveLoad, MemWarn, MemCritical,
Swapping, ThermalFair, ThermalSerious, OnBattery, BatteryLow, LowPowerMode,
UserDnd, InCall, Fullscreen, Presenting, MonitoringDisabled, Stale, WarmingUp,
ProbeError, LowConfidence, Headroom`). Free text only in a `detail` field.

## 8. Edge cases

- **Sleep/wake**: tick counters are not advancing during sleep but wall time is;
  measure with a monotonic clock that *includes* sleep (`mach_continuous_time`) and one
  that excludes it (`mach_absolute_time`); a jump between them = wake → reset deltas,
  `desconocido(WarmingUp)` for ~3 samples. Optionally subscribe to
  `IORegisterForSystemPower` (needs CFRunLoop) [primary objc2-io-kit binding].
- **Rate-limited cache** (`host_statistics*`): identical counters twice → mark sample
  `stale`, don't feed EWMA.
- **Clock / counter wrap**: 32-bit ticks wrap → `wrapping_sub`; if Δall ≤ 0 skip.
- **Probe errors**: N consecutive failures → `desconocido(ProbeError)`; never keep
  publishing an old `libre`.
- **Self-load**: if own CPU > budget (e.g. 0.5% over 10 min) → back off sampling
  interval ×2 and log; the sensor must never become the load.

## 9. Testing (Linux CI is fine — core is pure)

- Property tests: no state change faster than dwell; libre never emitted when any
  input stale; monotonic escalation on guard inputs.
- Replay traces: record `RawSample` streams on the M2 (JSON lines), replay through the
  classifier, assert transition counts (flap rate < 1 per 10 min under bursty load).
