# Measurement on the M2 and the per-slice efficiency gate

Nothing here has been run on a Mac for this skill. Tool flags marked [secondary] must be checked against
`man <tool>` / `<tool> -h` on the M2 the first time.

## 1. Why wakeups are the real idle cost

* Apple: "Whenever a timer fires, the CPU and various other systems wake from their low-power, idle states,
  incurring an energy cost"; "fixed cost" = bringing the system up and back down, "dynamic cost" = the work
  [primary, Energy Efficiency Guide for Mac Apps (archived)].
* Apple's own `top` turns this into a number: `POWER = (cpu_µs + idle_wakeups × 500 µs) / elapsed_µs × 100`
  (`top/power.c`, `taxed_us = idlew * 500`) [primary]. So **10 package-idle wakeups/s weigh as much as 0.5 % CPU**,
  even if each wakeup does nothing. A daemon at 0.05 % CPU with 20 wakeups/s scores ≈1.05 "POWER".
* powermetrics `tasks` sampler shows per process: CPU ms/s, User%, "Wakeups (Intr, Pkg idle)" — pkg-idle wakeups
  wake the whole package, are more expensive and a subset of Intr — and "Deadlines (<2 ms, 2-5 ms)" for short
  timers [secondary: Mozilla's powermetrics guide].

## 2. Tools

| Tool | Gives | Root | Notes |
|---|---|---|---|
| **in-process / helper `proc_pid_rusage`** | `ri_pkg_idle_wkups`, `ri_interrupt_wkups`, user/sys time, `ri_phys_footprint` (+ lifetime/interval max), `ri_cpu_time_qos_*`; v6 adds `ri_user_ptime`/`ri_system_ptime` (P-core time), `ri_pcycles`, **`ri_energy_nj`, `ri_penergy_nj`** | no (same uid) | exact counters, the gate's primary source. libc 0.2.191 ships v0–v4 structs; v6 needs a hand-written `#[repr(C)]` mirror of XNU `bsd/sys/resource.h` (§6). Time units: verify (mach ticks vs ns) |
| `top -l N -s S -pid P -stats pid,command,cpu,idlew,power,mem,threads,csw,time` | sampled CPU %, idle wakeups, POWER score, footprint, threads | no | keys from `top/preferences.c` [primary]; check whether `idlew` is per-interval in logging mode |
| `sudo powermetrics --samplers tasks --show-process-energy [--show-process-qos] -i 60000 -n 10 [-f plist] [-o file]` | per-process CPU ms/s, wakeups, energy impact (rough proxy across CPU/GPU/disk/net), QoS time | **yes** | flags per man page [secondary]; `--samplers cpu_power` gives cluster power (E/P) [secondary]; impact numbers are not comparable across machines |
| `xcrun xctrace record --template 'Time Profiler' --attach <pid> --time-limit 60s --output x.trace` | CPU stacks | no for own process | templates incl. "Time Profiler", "System Trace", "Allocations", "Leaks", "Activity Monitor" (`xctrace list templates`) [secondary via cargo-instruments 0.4.18]; `--attach` flag: verify with `xctrace help record` |
| Instruments **System Trace** | thread states, context switches, which thread woke whom, QoS per thread | no | the tool to find *who* wakes us |
| Instruments **Power Profiler** | per-subsystem power impact | — | **iPhone/iPad (iOS/iPadOS 26) only** per Apple docs → not for the Mac daemon [primary] |
| `samply record -p <pid>` (0.13.1) | sampling profiler, on- and off-CPU on macOS, Firefox Profiler UI | no; run `samply setup` once to attach on macOS | [primary README]; cannot profile Apple-signed system binaries |
| DHAT (`dhat` 0.3.3) | heap profile / heap-usage tests | no | release builds; experimental crate |
| `footprint`, `vmmap --summary`, `heap`, `leaks` | memory breakdown, live objects, leaks | no (own process) | [secondary]; `MallocStackLogging=1` for allocation backtraces (malloc(3)) |
| `pmset -g assertions` | power assertions held | no | daemon must hold none while idle |
| `memory_pressure -S -l warn` | simulated pressure | likely root [unverified] | primary man page |
| `taskpolicy -p <pid> -b / -B / -c background` | A/B QoS on a running process | own process: [unverified] | primary man page |

## 3. Idle test protocol (10 minutes, per slice)

Preconditions (record them in the result file; abort if not met):
1. Same M2, same macOS build (`sw_vers`), on **AC power**, Low Power Mode off, lid open, no external display
   changes, Wi-Fi on and same network. Record `pmset -g therm` and battery state.
2. No other heavy processes (`top -l 1 -o cpu -n 5` shows nothing > 5 % for 30 s before start).
3. Release build of the slice (same profile as production), production config, real or local echo server
   reachable (the connection is part of idle!).
4. Prevent system sleep for the duration without touching the daemon: `caffeinate -i -w <pid>` [secondary].

Procedure:
1. Start daemon; wait until "ready"; **warm-up 120 s** (connection established, caches settled).
2. Snapshot counters S0 (helper: rusage; top first sample; optional powermetrics).
3. Hands-off for **600 s**, sampling every 30 s (21 samples): CPU, idle/interrupt wakeups, footprint, threads.
4. Snapshot S1. Compute deltas and per-sample series.
5. Repeat the whole run **3 times** (fresh start each), report the **median** run; keep all raw data.
6. Once per milestone: a **burst-then-idle** run (inject standard workload for 60 s, then 600 s idle) and a
   **24 h soak** (sample every 5 min).

Metrics (all from S0→S1 unless noted):
* `cpu_pct` = (Δuser+Δsys)/600 s × 100 (one-core %)
* `idle_wakeups_per_s` = Δri_pkg_idle_wkups/600; `intr_wakeups_per_s` = Δri_interrupt_wkups/600
* `power_score` = cpu_pct + idle_wakeups_per_s × 0.05 (top's 500 µs tax)
* `footprint_max_mb`, `footprint_end_mb`, `footprint_slope_mb_per_h` (least squares over 21 samples)
* `threads_max`; `p_core_share` (v6: Δptime / Δtotal time) and `energy_mj` (v6) when available
* runtime `unparks_per_s` from tokio `on_thread_unpark` counter (cross-check)

## 4. Thresholds (working targets — calibrate after 3 baseline slices; they are not facts)

| Metric | Hard fail (absolute) | Regression fail (vs stored baseline) |
|---|---|---|
| cpu_pct (idle) | > 1.0 | > baseline × 1.25 **and** > baseline + 0.05 |
| idle_wakeups_per_s | > 2.0 (design target ≤ 0.5) | > baseline × 1.25 and > baseline + 0.2 |
| intr_wakeups_per_s | > 5.0 | > baseline × 1.25 and > baseline + 0.5 |
| footprint_end_mb | > 50 | > baseline × 1.15 and > baseline + 2 MB |
| footprint_slope_mb_per_h (10-min idle) | > 2 | — (noisy; use soak) |
| 24 h soak: idle-floor growth | > 5 MB/24 h | — |
| threads_max | > declared thread budget for the slice | any new thread without a declaration |
| power assertions while idle | any | — |
| burst-then-idle: footprint 600 s after burst − pre-burst | > 10 MB | > baseline + 5 MB |

The "and > baseline + floor" clause stops tiny baselines (0.02 wakeups/s) from failing on noise. Store
baselines as JSON in the repo (`perf/baselines/<slice>.json`), update only by an explicit, reviewed commit
that states why the cost increased.

## 5. Reference script

`references/idle-gate.sh` — **reference only, not auto-run, untested on macOS**. It implements §3/§4 with
`top` + optional helper + optional `sudo -n powermetrics`, writes `result.json` and exits non-zero on failure.

## 6. Helper: rusage reader (sketch for a dev-only binary `fogar-meter`)

```rust
// Mirror of XNU bsd/sys/resource.h `struct rusage_info_v6` (xnu @ f6217f8). Field order matters.
#[repr(C)] #[derive(Default)]
pub struct RusageInfoV6 {
    pub ri_uuid: [u8; 16],
    pub ri_user_time: u64, pub ri_system_time: u64,
    pub ri_pkg_idle_wkups: u64, pub ri_interrupt_wkups: u64,
    pub ri_pageins: u64, pub ri_wired_size: u64, pub ri_resident_size: u64, pub ri_phys_footprint: u64,
    pub ri_proc_start_abstime: u64, pub ri_proc_exit_abstime: u64,
    pub ri_child_user_time: u64, pub ri_child_system_time: u64,
    pub ri_child_pkg_idle_wkups: u64, pub ri_child_interrupt_wkups: u64,
    pub ri_child_pageins: u64, pub ri_child_elapsed_abstime: u64,
    pub ri_diskio_bytesread: u64, pub ri_diskio_byteswritten: u64,
    pub ri_cpu_time_qos_default: u64, pub ri_cpu_time_qos_maintenance: u64,
    pub ri_cpu_time_qos_background: u64, pub ri_cpu_time_qos_utility: u64,
    pub ri_cpu_time_qos_legacy: u64, pub ri_cpu_time_qos_user_initiated: u64,
    pub ri_cpu_time_qos_user_interactive: u64,
    pub ri_billed_system_time: u64, pub ri_serviced_system_time: u64,
    pub ri_logical_writes: u64, pub ri_lifetime_max_phys_footprint: u64,
    pub ri_instructions: u64, pub ri_cycles: u64,
    pub ri_billed_energy: u64, pub ri_serviced_energy: u64,
    pub ri_interval_max_phys_footprint: u64, pub ri_runnable_time: u64, pub ri_flags: u64,
    pub ri_user_ptime: u64, pub ri_system_ptime: u64, pub ri_pinstructions: u64, pub ri_pcycles: u64,
    pub ri_energy_nj: u64, pub ri_penergy_nj: u64,
    pub ri_secure_time_in_system: u64, pub ri_secure_ptime_in_system: u64,
    pub ri_neural_footprint: u64, pub ri_lifetime_max_neural_footprint: u64,
    pub ri_interval_max_neural_footprint: u64,
    pub ri_reserved: [u64; 9],
}
const RUSAGE_INFO_V6: libc::c_int = 6;
fn read_v6(pid: libc::pid_t) -> std::io::Result<RusageInfoV6> {
    let mut ri = RusageInfoV6::default();
    let rc = unsafe { libc::proc_pid_rusage(pid, RUSAGE_INFO_V6,
                      &mut ri as *mut RusageInfoV6 as *mut libc::rusage_info_t) };
    if rc == 0 { Ok(ri) } else { Err(std::io::Error::last_os_error()) }  // fall back to libc::rusage_info_v4
}
```
Untested: whether the M2's macOS version accepts flavor 6 (fall back to `libc::RUSAGE_INFO_V4` +
`libc::rusage_info_v4`), units of `*_time`, and whether `ri_energy_nj` is populated on M2.
`[derive(Default)]` on `[u64; 9]` works; `[u8; 16]` too.

## 7. Regression triage order

1. Wakeups up? → System Trace (who woke), `on_thread_unpark` counter, list armed timers (log every timer
   creation with its period in debug builds), check new deps for hidden threads.
2. CPU up with same wakeups? → samply / Time Profiler on the burst run.
3. Footprint up? → `footprint`/`vmmap --summary` diff between baseline and new run; DHAT on the burst workload;
   check caches' caps and channel capacities; allocator purge behaviour (memory-and-allocators.md).
4. Threads up? → `on_thread_start` logging with thread names; blocking pool size; dependency threads.
