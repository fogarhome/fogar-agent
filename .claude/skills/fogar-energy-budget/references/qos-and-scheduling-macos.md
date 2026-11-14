# QoS, P/E cores, timer coalescing and process role on macOS (Apple Silicon)

Labels: **[primary]** = read in Apple OSS source/headers or Apple docs; **[secondary]** = third-party write-up;
**[unverified]** = plausible, not confirmed; **[untested]** = nobody ran it on a Mac for this skill.
Everything macOS here is **untested on the M2**. Source snapshot: apple-oss-distributions/xnu @ f6217f8
(Oct 2025), libpthread @ 42d026d, libdispatch @ 2361ffb.

## 1. The QoS classes (libpthread `sys/qos.h`) [primary]

| Constant | Value | Header intent (paraphrased) |
|---|---|---|
| `QOS_CLASS_USER_INTERACTIVE` | 0x21 | UI event loop / drawing. "not an energy-efficient QOS class" |
| `QOS_CLASS_USER_INITIATED` | 0x19 | user is waiting. Not energy-efficient for large tasks |
| `QOS_CLASS_DEFAULT` | 0x15 | what `pthread_create()` threads get when no QoS attribute is set; "not intended to be used as a work classification" |
| `QOS_CLASS_UTILITY` | 0x11 | "should be run in an energy and thermally-efficient manner" |
| `QOS_CLASS_BACKGROUND` | 0x09 | "run in the most energy and thermally-efficient manner" |
| `QOS_CLASS_UNSPECIFIED` | 0x00 | absence / opted out |

APIs (`pthread/qos.h`, all exposed by the `libc` crate 0.2.191 under `target_vendor = "apple"`):
`pthread_set_qos_class_self_np(qos_class_t, relpri)`, `pthread_attr_set_qos_class_np`,
`pthread_get_qos_class_np`. `relpri` must be in `[QOS_MIN_RELATIVE_PRIORITY, 0]`; use `0`.

**Trap [primary]:** "Subsequent calls to interfaces such as `pthread_setschedparam()` that are incompatible
or in conflict with the QOS class system will unset the QOS class ... A thread so modified is
permanently opted-out of the QOS class system and calls to this function ... will fail and return EPERM."
Apple's Apple-silicon tuning article says the same: replace `pthread_setschedparam`, `setpriority`,
`thread_set_policy` with QoS APIs. The `thread-priority` crate (3.1.1) uses `pthread_setschedparam` on Unix →
**do not use it on macOS threads you want QoS on.**

Rust (sketch; compiles against libc 0.2.191 on apple targets, untested):
```rust
#[cfg(target_vendor = "apple")]
fn set_self_qos(class: libc::qos_class_t) -> std::io::Result<()> {
    let rc = unsafe { libc::pthread_set_qos_class_self_np(class, 0) };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::from_raw_os_error(rc)) }
}
// e.g. set_self_qos(libc::qos_class_t::QOS_CLASS_UTILITY)
```

## 2. How QoS steers P vs E cores on M1/M2 [primary + secondary]

* On arm64 with `__AMP__` XNU defines `CONFIG_SCHED_EDGE` (Clutch + Edge scheduler) (`osfmk/kern/sched.h`).
  Cluster choice for each QoS bucket of a thread group comes from CLPC (the closed-source performance
  controller): "the Edge scheduler allows CLPC to specify a preferred cluster for each QoS level in a TG"
  (`sched_clutch.c`). [primary]
* Only hard rule visible in open source (`sched_edge_qos_max_parallelism`): "The only restriction is for
  BG/Maintenance QoS classes for which the performance controller would never recommend execution on the
  P-cores." Utility "can be scheduled on all CPUs". [primary]
* Older non-Edge AMP path (`sched_amp_common.c`) restricted both Utility and BG to E cores by default — do
  **not** assume that on M2. [primary, but not the M2 code path]
* Eclectic Light (M-series observations): BG (9) runs only on E cores and cannot be promoted; QoS ≥ 17
  prefers P cores and spills to E when P is busy. [secondary] Matches the XNU comment for BG; for Utility it
  says "prefers P" whereas XNU only says "may run anywhere" → **measure Utility placement on the M2.**
* Measured energy example (M1 Max, 1 GB compression): BG/E-core run 3.1 J in ~5 s vs high-QoS/P-core run
  10.3 J in 0.6 s. [secondary] → for *throughput* work BG saves energy but takes ~8x longer.

**Default QoS = no information.** Threads created by Rust `std::thread`/tokio without a QoS call run at
`QOS_CLASS_DEFAULT` (between UI and Utility): eligible for P cores, I/O tier 0, short timer leeway.

## 3. Side effects of BACKGROUND QoS (beyond E cores) [primary, `thread_policy.c`]

| QoS | base pri | max I/O tier | throughput tier | latency tier |
|---|---|---|---|---|
| USER_INTERACTIVE | 46 | 0 | 0 | 0 |
| USER_INITIATED | BASEPRI_USER_INITIATED | 0 | 1 | 1 |
| DEFAULT (legacy) | BASEPRI_DEFAULT | 0 | 1 | 1 |
| UTILITY | BASEPRI_UTILITY | **1** | 2 | 3 |
| BACKGROUND | MAXPRI_THROTTLE | **2** | 5 | 3 |
| MAINTENANCE | MAXPRI_THROTTLE | 3 | 5 | 3 |

* A thread whose effective QoS is BACKGROUND/MAINTENANCE becomes **darwinbg**: `if (next.thep_qos ==
  THREAD_QOS_BACKGROUND || ...) wants_darwinbg = true;` and "darwinbg threads always create bg sockets".
  `PRIO_DARWIN_BG` (resource.h): "lowers CPU, disk IO, and networking priorities". **Consequence:** a socket
  (e.g. the WebSocket) *created on a BG thread* is a background-traffic socket. Keep network-owning threads at
  UTILITY (or DEFAULT), put only deferrable CPU/disk batch work at BG.
* I/O tier 2 = throttled disk I/O behind foreground I/O. Good for compaction/indexing, bad for the hot path.

## 4. Timer coalescing / leeway [primary source reading, untested]

XNU adds slop `min((deadline-now) >> shift, max)` (`timer_call.c` `timer_call_slop`), scaled up further by
`timer_user_idle_level`. macOS arm64 parameters (`osfmk/arm/arm_timer.c`):

| Class of the waiting thread / urgency | shift | cap |
|---|---|---|
| user timer, normal timeshare thread | 3 (1/8 of delta) | 1 ms |
| user timer, latency-QoS tier 1 (DEFAULT/USER_INITIATED) | 2 (1/4) | 5 ms |
| user timer, latency tier 3 (UTILITY/BG) | −2 (×4) | 75 ms |
| user timer on a darwinbg (BG) thread or `*_USER_BACKGROUND` urgency | −5 (×32) | 100 ms |
| **system-urgency timeouts (kernel `assert_wait_deadline`)** | 3 | 1 ms |

**Important for tokio:** mio on macOS parks in `kevent(..., timeout)` (`mio/src/sys/unix/selector/kqueue.rs`),
and `kqueue_scan` waits via `assert_wait_deadline(...)`, which uses `TIMEOUT_URGENCY_SYS_NORMAL`
(`sched_prim.c`). By the table above that gets ≤1 ms slop *regardless of the thread's QoS*. So setting BG QoS
on a tokio worker does **not** make tokio timers coalesce more. `select()` uses `TIMEOUT_URGENCY_USER_NORMAL`
instead (QoS-sensitive). → Coalescing for tokio must be done **in the application** (align deadlines, one
heartbeat; see `tokio-low-idle.md`). If an OS-coalesced timer is needed, use a GCD timer:
`dispatch_source_set_timer(src, start, interval, leeway)` — repeat fires get leeway `MIN(leeway, interval/2)`;
avoid `DISPATCH_TIMER_STRICT` ("may override power-saving techniques") [primary, libdispatch `source.h`], or a
kqueue `EVFILT_TIMER` with `NOTE_LEEWAY`/`NOTE_BACKGROUND` (`bsd/sys/event.h`). Apple guide: tolerance ≥10%
of the interval [primary, archived Energy Efficiency Guide].

## 5. Process-level role

* **launchd `ProcessType`** (launchd.plist(5), launchd-842 OSS copy; Apple's Apple-silicon article says
  "Always include the `ProcessType` key"): `Background` (resource limits "to prevent them from disrupting the
  user experience"), `Standard` (= unset; "light resource limits ... throttling its CPU usage and I/O
  bandwidth"), `Adaptive` (moves between Background and Interactive based on XPC activity), `Interactive`
  (no limits, only if responsiveness depends on it). `LowPriorityIO` key also exists. [primary]
  Unknown: whether `Background` makes the whole task darwinbg (→ background sockets for the WebSocket).
  **Measure** before choosing; candidate: `Adaptive` if the UI talks to the daemon over XPC, else compare
  `Background` vs `Standard` on WebSocket latency + energy.
* **`taskpolicy(8)`** [primary, system_cmds]: `-b` (PRIO_DARWIN_BG), `-B` (undo), `-c utility|background|maintenance`
  (QoS clamp at spawn), `-d/-g` disk I/O policy, `-t`/`-l` throughput/latency tier, `-p pid` to change a running
  process, `-m` memory limit (MiB). Use it **as a measurement lever** (A/B a running daemon without rebuilding),
  not as the production mechanism.
* `setpriority(PRIO_DARWIN_THREAD, 0, PRIO_DARWIN_BG)` / `PRIO_DARWIN_PROCESS` — in libc crate; implies BG QoS
  and background sockets. Prefer QoS API.
* **App Nap** applies to apps (NSProcessInfo activities, `NSActivityBackground`, `NSActivityIdleSystemSleepDisabled`
  in objc2-foundation 0.3.2). A launchd daemon/agent without UI is governed by `ProcessType`, not App Nap
  [unverified]. Never hold `NSActivityIdleSystemSleepDisabled` or IOPM assertions while idle; check with
  `pmset -g assertions`.
* XPC note [primary, Apple article]: work done for an app over XPC inherits the app's importance; plain sockets
  lose that. Relevant only if a UI client talks to fogar-agent.

## 6. Recommended thread map for fogar-agent (to validate on M2)

| Thread role | QoS | Why |
|---|---|---|
| tokio workers (net I/O, protocol, timers) | UTILITY via `on_thread_start` | energy-efficient, not darwinbg → normal sockets, short I/O tier |
| tokio blocking pool (fs, DNS) | inherits UTILITY (same hook) | `on_thread_start` runs for workers *and* blocking threads |
| deferrable batch CPU/disk (compaction, indexing, uploads that can wait) | BACKGROUND on a dedicated `std::thread` (or set/restore inside the `spawn_blocking` closure) | E-cores only, throttled I/O |
| anything user-visible with a deadline (if ever) | USER_INITIATED for the duration, then restore | — |

Never share a `Mutex` between a BG thread and a latency-sensitive thread (priority inversion; BG may sit on a
saturated E-cluster). Hand work over channels instead.
