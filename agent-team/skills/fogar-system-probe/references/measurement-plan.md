# What to measure on the MacBook Air M2 (nothing here was measured yet)

Run all as the normal user unless marked (root). Record macOS version + build each time.

## A. Micro-cost of each probe (build a tiny bench binary, 10 000 iterations each)

| Call | Expectation [unverified] | Accept if |
|---|---|---|
| `host_processor_info` + `vm_deallocate` | ~µs | < 20 µs |
| `host_statistics64(HOST_VM_INFO64)` | ~µs; **cached after 2–10 calls/s** | check repeated identical results |
| `sysctlbyname("kern.memorystatus_level")`, `vm.swapusage`, `kern.memorystatus_vm_pressure_level` | ~1–3 µs | < 10 µs |
| `proc_pid_rusage(getpid(), RUSAGE_INFO_V6)` | ~µs | < 10 µs; also check V6 accepted |
| IORegistry `HIDIdleTime` (cached service, single-key) | tens of µs | < 100 µs |
| `IOPSCopyPowerSourcesInfo` + description | tens–hundreds of µs (XPC to powerd?) | < 1 ms; else event-only |
| `IOPMCopyAssertionsStatus` | likely XPC round trip | < 1 ms; else only when user active |
| `NSProcessInfo.thermalState` | getter | < 50 µs |
| IOAccelerator `PerformanceStatistics` | dictionary copy | < 200 µs |
| full process scan (listallpids + TASKINFO per pid) | few ms for ~600 pids | < 10 ms |
| `CGWindowListCopyWindowInfo(onScreenOnly)` | ms | only when user active |

Measure with `mach_absolute_time` deltas ×timebase. Also note which calls fail (EPERM)
for other-uid PIDs — confirm the same-uid rule.

## B. Steady-state daemon budget (run 72 h)

- CPU: `top -l 0 -s 60 -stats pid,cpu,idlew,power -pid <pid>`; target < 0.1% avg, sensor <1% hard cap.
- Wakeups: `idlew` column in top, or `powermetrics --samplers tasks --show-process-energy -i 60000` **(root)**; target < 1 wake/s average (event-driven + coalesced timers should give ≈ 1 per sampling interval).
- Memory: `footprint <pid>` / `ri_phys_footprint`; target < 15 MB for the monitor slice (whole agent < 50 MB).
- Energy: Activity Monitor Energy tab ("Energy Impact", "12 hr Power") + own `ri_energy_nj` delta per hour (if non-zero on M2).
- Self-telemetry: log hourly own `ri_user_time+ri_system_time`, `ri_pkg_idle_wkups`, `ri_interrupt_wkups`, `ri_phys_footprint`, `ri_energy_nj`.

## C. Behavioural facts to establish

1. `cpuN` ↔ perflevel mapping on M2 (pin a busy loop with QoS background vs
   user-interactive and watch which tick indices rise).
2. Thermal: on the fanless Air, time from cold to `Fair`/`Moderate` and to `Serious`
   under (a) 4 threads, (b) 8 threads; compare `NSProcessInfo.thermalState` vs notify
   pressure level vs `powermetrics --samplers thermal` (root).
3. Memory pressure: simulate with `memory_pressure -l warn` / `-l critical`
   [secondary: tool exists on macOS] — verify dispatch event latency and the sysctl
   value; check what the agent sees during heavy swap.
4. `HIDIdleTime` during: SSH-only activity (should keep growing), Screen Sharing remote
   input, video playback, external keyboard.
5. `PreventUserIdleDisplaySleep` count during YouTube in Safari, Keynote play, Zoom/Meet
   call, `caffeinate -d`.
6. Camera/mic "running somewhere" properties during a call; does querying trigger TCC?
7. Sleep/wake: tick counters and `mach_continuous_time` vs `mach_absolute_time` across
   lid close; first-sample garbage?
8. host_statistics rate-limit: call 20×/s and record how often values repeat.
9. `ri_energy_nj` / `ri_penergy_nj` non-zero for own-uid processes? Compare with
   Activity Monitor energy ordering.
10. Classifier replay: record 7 days of RawSample JSONL during normal use; tune
    thresholds so false `libre` during active work ≈ 0 and flaps < 1/10 min.
11. Delegated-job impact: run a 4-core compile via SSH with and without
    `taskpolicy -b` while the user works; measure UI jank (subjective) and thermal.
12. macOS 27-specific: re-check every API above on the shipping macOS (27.x per macmon
    notes) — especially notification-based ones.
