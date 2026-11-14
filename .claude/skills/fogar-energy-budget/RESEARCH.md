# RESEARCH — fogar-energy-budget

Research date: 2026-11-12. Researcher could not run macOS: every macOS behaviour below is **untested**.
Network: only GitHub, crates.io and a few Apple doc URLs (via search results) were reachable;
developer.apple.com HTML pages, docs.rs, kernel.org, learn.microsoft.com and man-page mirrors were mostly
blocked, so primary material was read from **source repositories** (apple-oss-distributions, tokio-rs,
MicrosoftDocs/sdk-api, torvalds/linux, systemd) and **crate tarballs** from static.crates.io.
All downloads were shallow/sparse clones or tarballs into empty directories; nothing downloaded was executed
(the only code run was my own Linux experiment, which compiles the tokio crate).

Labels: **P** = primary (source/header/official doc read), **S** = secondary, **U** = unverified,
**L** = measured on Linux by this research.

## 1. Sources

### Apple OSS (github.com/apple-oss-distributions)
| Repo @ commit | Files read |
|---|---|
| xnu @ f6217f8 (2025-10-16) | `osfmk/kern/sched.h` (CONFIG_SCHED_EDGE on AMP), `osfmk/kern/sched_clutch.c` (Edge preferred cluster, `sched_edge_qos_max_parallelism`), `osfmk/kern/sched_amp_common.c` (non-Edge AMP policy), `osfmk/kern/thread_policy.c` (QoS → pri/iotier/latency tables, darwinbg/sockets), `osfmk/kern/timer_call.c` (`timer_call_slop`, `timer_compute_leeway`), `osfmk/arm/arm_timer.c` (arm64 coalescing params), `osfmk/kern/sched_prim.c` (`assert_wait_deadline` urgency), `bsd/kern/kern_event.c` (kqueue_scan wait, EVFILT_TIMER flags), `bsd/kern/sys_generic.c` (select urgency), `bsd/sys/event.h` (NOTE_LEEWAY/BACKGROUND/CRITICAL), `bsd/sys/resource.h` (PRIO_DARWIN_BG, rusage_info v0–v6), `bsd/kern/kern_resource.c` (PRIO_DARWIN_BG socket traffic class), `osfmk/kern/bsd_kern.c` (`fill_task_rusage`), `osfmk/kern/task.c` (phys_footprint ledger definition), `osfmk/mach/task_policy.h` (task roles, latency tiers) — https://github.com/apple-oss-distributions/xnu |
| libpthread @ 42d026d | `include/sys/qos.h`, `include/pthread/qos.h` — https://github.com/apple-oss-distributions/libpthread |
| libdispatch @ 2361ffb | `dispatch/source.h` (memorypressure flags + guidance, timer leeway, DISPATCH_TIMER_STRICT) — https://github.com/apple-oss-distributions/libdispatch |
| libmalloc @ c49dafa | `doc/xzone_malloc.md`, `include/malloc/malloc.h` (`malloc_zone_pressure_relief`), `man/malloc.3` — https://github.com/apple-oss-distributions/libmalloc |
| system_cmds @ 408bba7 | `taskpolicy/taskpolicy.8`, `taskpolicy.c`, `memory_pressure/memory_pressure.1` — https://github.com/apple-oss-distributions/system_cmds |
| launchd @ 842.1.4 (2021 OSS drop) | `man/launchd.plist.5` (ProcessType, LowPriorityIO) — https://github.com/apple-oss-distributions/launchd (old; current launchd is closed) |
| Libc @ 71bbe35 | `include/libkern/OSThermalNotification.h` — https://github.com/apple-oss-distributions/Libc |
| Libnotify (HEAD) | `notify.h`, `notify_keys.h` — https://github.com/apple-oss-distributions/Libnotify |
| IOKitUser (HEAD) | `ps.subproj/IOPowerSources.h` — https://github.com/apple-oss-distributions/IOKitUser |
| top @ 1e3b6ca | `top.1`, `power.c` (POWER score formula), `preferences.c` (stat keys), `libtop.c` — https://github.com/apple-oss-distributions/top |

### Apple documentation (fetched)
* Tuning your code's performance for Apple silicon — https://developer.apple.com/tutorials/data/documentation/apple-silicon/tuning-your-code-s-performance-for-apple-silicon.md (P)
* Measuring your app's power use with Power Profiler — https://developer.apple.com/tutorials/data/documentation/xcode/measuring-your-app-s-power-use-with-power-profiler.md (P: iPhone/iPad only)
* Energy Efficiency Guide for Mac Apps (archived): Fundamental Concepts — https://developer-mdn.apple.com/library/archive/documentation/Performance/Conceptual/power_efficiency_guidelines_osx/FundamentalConcepts.html ; Best Practices — https://developer-mdn.apple.com/library/archive/documentation/Performance/Conceptual/power_efficiency_guidelines_osx/BestPractices.html (P, archived)
* Energy Efficiency Guide for iOS Apps, Minimize Timer Use — https://developer-mdn.apple.com/library/archive/documentation/Performance/Conceptual/EnergyGuide-iOS/MinimizeTimerUse.html (P, archived; 10 % tolerance)
* Not fetched (blocked): Mac guide "Timers", "Prioritize Work at the Task Level", "App Nap" pages; NSProcessInfo thermalState page; WWDC energy sessions. Their claims are only used where confirmed by headers/source.

### Rust ecosystem
* tokio master @ ab0814f = 1.54.1 (latest crates.io 2026-11-12): `runtime/builder.rs`, `runtime/time/mod.rs`, `runtime/time/wheel/level.rs`, `time/interval.rs`, `sync/mpsc/{list,block,unbounded}.rs`, `loom/std/mod.rs` — https://github.com/tokio-rs/tokio (P)
* Crates read from static.crates.io tarballs (P): libc 0.2.191 (`src/new/apple/libpthread/*`, `unix/bsd/apple/mod.rs`), mio 1.2.4 (`sys/unix/selector/kqueue.rs`), tungstenite 0.30.0 / tokio-tungstenite 0.30.0 (`protocol/mod.rs`), mimalloc 0.1.52 + libmimalloc-sys 0.1.49 (bundled mimalloc v3.3.2 / v2.3.2: `options.c`, `prim/unix/prim.c`, `arena.c`, `build.rs`), tikv-jemallocator 0.7.0 + tikv-jemalloc-sys 0.7.1+5.3.1 (`build.rs`, `src/env.rs`, `jemalloc/configure.ac`, `TUNING.md`, `doc/jemalloc.xml.in`, `src/pages.c`), tikv-jemalloc-ctl 0.7.0, dispatch2 0.3.1, objc2-foundation 0.3.2 (`generated/NSProcessInfo.rs`), dhat 0.3.3, thread-priority 3.1.1 (`unix.rs`), cargo-instruments 0.4.18 (`instruments.rs`). Latest versions also seen: samply 0.13.1, cargo-pgo 0.3.0.
* Cargo reference, profiles — https://github.com/rust-lang/cargo/blob/master/doc/book/src/reference/profiles.md (P)
* cargo-pgo README — https://github.com/Kobzol/cargo-pgo (P for tool); samply README — https://github.com/mstange/samply (P); perf-book — https://github.com/nnethercote/perf-book (S)

### Windows / Linux
* MicrosoftDocs/sdk-api @ 656e965: `nf-processthreadsapi-setprocessinformation.md`, `nf-processthreadsapi-setthreadinformation.md`, `ns-processthreadsapi-thread_power_throttling_state.md`, `nf-processthreadsapi-setpriorityclass.md`, `nf-memoryapi-creatememoryresourcenotification.md`, `nf-powersetting-powerregisterforeffectivepowermodenotifications.md` — https://github.com/MicrosoftDocs/sdk-api (P)
* Linux @ 7e1392a: `Documentation/accounting/psi.rst`, `Documentation/admin-guide/cgroup-v2.rst` — https://github.com/torvalds/linux (P)
* man-pages GitHub mirror (snapshot 2021): `man2/prctl.2` PR_SET_TIMERSLACK — https://github.com/mkerrisk/man-pages (P, old snapshot)
* systemd @ 9571d66: `man/systemd.exec.xml`, `man/systemd.resource-control.xml` — https://github.com/systemd/systemd (P)

### Secondary
* Eclectic Light, "What is Quality of Service, and how does it matter?" — https://eclecticlight.co/2025/05/09/what-is-quality-of-service-and-how-does-it-matter/
* Eclectic Light, "Running tasks on E cores can use a third of the energy of P cores" — https://eclecticlight.co/2022/05/03/running-tasks-on-e-cores-can-use-a-third-of-the-energy-of-p-cores/
* Mozilla, powermetrics guide — https://firefox-source-docs.mozilla.org/performance/powermetrics.html
* powermetrics(1) mirror — https://keith.github.io/xcode-man-pages/powermetrics.1.html

## 2. Claims and confidence

| # | Claim | Label | Confidence |
|---|---|---|---|
| 1 | Idle tokio (ct or mt) with no timers: 0 wakeups/s; waiting on mpsc/socket free | L | high (Linux); mac: expect same (kqueue park w/o timeout), untested |
| 2 | tokio timers cascade: 1 s → ~2 wakeups/tick, 30 s → ~3; caused by `next_expiration` = slot start at timer's level | P + L | high |
| 3 | Aligning N timers to same phase collapses wakeups (50 → cost of 1) | L | high (Linux) |
| 4 | Multi-thread runtime: 1 wakeup per small incoming message, same as current_thread | L | medium (2 vCPU VM, light load) |
| 5 | `disable_lifo_slot` is unstable and a throughput knob | P | high |
| 6 | defaults: workers = available_parallelism, blocking 512, keep-alive 10 s, event_interval 61 | P | high |
| 7 | Edge scheduler on M-series; BG/Maintenance never recommended on P cores; Utility may run anywhere per CLPC | P | high for code; CLPC behaviour closed |
| 8 | BG QoS ⇒ darwinbg ⇒ new sockets background, I/O tier 2 | P | high (code); network effect size U |
| 9 | `pthread_setschedparam` permanently opts out of QoS | P | high |
| 10 | Default pthread QoS = QOS_CLASS_DEFAULT | P | high |
| 11 | kevent waits get ≤1 ms slop regardless of QoS (SYS_NORMAL urgency) | P (source reading) | medium — reading of several call paths; verify with powermetrics "Deadlines" or System Trace |
| 12 | arm64 macOS leeway params (ts 1/8 cap 1 ms; BG ×32 cap 100 ms; latency tiers) | P | high for values; mapping to Rust timers via kevent = see 11 |
| 13 | top POWER = (cpu + 500 µs × idle wakeups)/elapsed | P | high |
| 14 | phys_footprint formula; top `mem` = footprint | P | high |
| 15 | rusage v6 exposes P-core time and energy (nJ) | P (header + kernel fill) | high for struct; availability on M2's macOS version U |
| 16 | xzone malloc deferred reclaim; mimalloc-derived | P | high; default-for-3rd-party U |
| 17 | mimalloc v3 default in crate, purge_delay 1000 ms, purges in allocator calls, uses MADV_FREE_REUSABLE on macOS | P | high |
| 18 | jemalloc background threads disabled on Mach-O; uses MADV_FREE | P | high |
| 19 | Memory pressure guidance: change future behaviour, don't walk caches | P | high |
| 20 | Thermal notify name + macOS levels; IOPS notify keys | P | high (headers) |
| 21 | ProcessType values and semantics | P (old man page) + P (Apple article "always include ProcessType") | medium (man page from 2014-era OSS) |
| 22 | App Nap does not govern launchd daemons | U | low–medium |
| 23 | E-core run used ~30 % of the energy of the P-core run (M1 Max) | S | medium |
| 24 | Power Profiler is iPhone/iPad only | P | high (doc as of 2026) |
| 25 | BOLT not usable on Mach-O | S/U | medium |
| 26 | powermetrics needs root; tasks sampler columns | S | high (common knowledge, man page mirror) |
| 27 | xctrace `--attach`, footprint/vmmap/heap/leaks flags | S/U | verify on M2 |
| 28 | Windows EcoQoS semantics, IGNORE_TIMER_RESOLUTION, effective power mode API | P | high |
| 29 | PSI unprivileged trigger window multiple of 2 s; timer slack default 50 µs | P | high |
| 30 | `ri_*_time` units are mach ticks on Apple silicon | S | medium |

## 3. Conflicts / tensions recorded

1. **Race-to-idle vs E-cores.** Throughput work on BG/E cores took ~8x longer but ~30 % of the energy (S, M1 Max).
   For tiny event handlers the fixed wake cost dominates and core type matters little → reduce wakeups first,
   then use BG for bulk. Measure both on the M2 for our real batch jobs.
2. **Utility placement.** Eclectic Light: QoS ≥17 *prefers* P cores; XNU Edge comment: Utility *may* run anywhere
   per CLPC; non-Edge AMP code restricts Utility to E. M2 runs Edge → don't promise E-core placement for UTILITY.
3. **"BG timers coalesce more"** (Apple guidance, XNU BG shift −5/100 ms) vs tokio's kevent waits that use
   system urgency (≤1 ms). The OS coalescing benefit does not reach tokio timers; app-level alignment needed.
4. **Allocator folklore** ("use jemalloc/mimalloc for long-running servers") vs macOS reality: system xzone has
   kernel-integrated reclaim; jemalloc lacks background purging on macOS; mimalloc purges only on activity.
5. **Activity Monitor/`%CPU` vs energy**: Eclectic Light shows Activity Monitor's energy pane ranking the
   P-core run as cheaper, the reverse of powermetrics (S). Gate on counters, not on Activity Monitor.
6. **panic="abort"**: smaller/faster vs losing tokio's per-task panic isolation.
7. Apple's Mac Energy guide (archived, App-centric) vs headless daemon: App Nap / UI-visibility parts don't apply.

## 4. Open questions

* Does `ProcessType=Background` (or `Standard`'s "light limits") make the task darwinbg and therefore the
  WebSocket a background-class socket? Latency/energy impact?
* Does XNU's `timer_user_idle_level` or any other path widen kevent timeouts when the user is idle?
* Is xzone malloc the default zone for a third-party arm64 binary on the M2's macOS version?
* Does `proc_pid_rusage(..., RUSAGE_INFO_V6)` work and populate `ri_energy_nj` on M2? Units of `ri_*_time`?
* Does tokio multi-thread wake additional workers under real bursts on 4P+4E (work-stealing "searching" wakes)?
* Exact `top -l` semantics of `idlew` (per interval vs cumulative) in logging mode.
* Is `memory_pressure -S` usable without root?
* Windows: a per-app thermal signal (none found); Linux: event-driven power-source signal without D-Bus.

## 5. Must be measured on the real M2 (checklist)

1. Repeat the Linux tokio matrix (idle ct/mt, 1 s/30 s intervals, N staggered vs aligned timers, TCP msgs) with
   `ri_pkg_idle_wkups`/`ri_interrupt_wkups` → confirm cascade and alignment effects under kqueue.
2. QoS placement: run a CPU loop at DEFAULT/UTILITY/BACKGROUND; read `ri_user_ptime/ri_system_ptime` vs total
   and `powermetrics --show-process-qos`; same with `taskpolicy -c background`.
3. Timer slop: powermetrics "Deadlines (<2 ms, 2–5 ms)" and System Trace for the 30 s heartbeat at
   UTILITY vs BACKGROUND.
4. Background sockets: WebSocket RTT and throughput from a BG thread vs UTILITY thread; and under each
   launchd ProcessType.
5. Allocator A/B (system / mimalloc v3 / + `mi_collect`) with the burst-then-idle protocol; jemalloc only if
   requested.
6. `phys_footprint` baseline of the empty daemon (runtime + TLS + WebSocket) → calibrate the 50 MB target.
7. Idle gate baselines for the first 3 slices (3 runs each) → calibrate thresholds/noise floors.
8. Thermal: sustained load on the fanless M2 → time to `Fair/Serious`, and that shedding reverses it.
9. Memory pressure: `memory_pressure -S -l warn|critical` → handler fires, footprint response.
10. Startup: cold start time and footprint at "ready" for `panic="abort"` vs `"unwind"`, `lto` variants.
11. 24 h soak: idle floor drift, wakeup stability, reconnect behaviour across system sleep/wake
    (`MissedTickBehavior::Delay` check).
