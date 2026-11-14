# Memory on macOS: footprint vs RSS, allocators, returning memory, fragmentation over days

All macOS behaviour **untested on the M2**. Labels as in SKILL.md.

## 1. What to measure: `phys_footprint`, not RSS [primary]

XNU `osfmk/kern/task.c` ledger comment:
```
phys_footprint = (internal - alternate_accounting)
               + (internal_compressed - alternate_accounting_compressed)
               + iokit_mapped + purgeable_nonvolatile + purgeable_nonvolatile_compressed + page_table
```
* Includes **compressed** anonymous memory (RSS does not) and excludes clean file-backed pages such as the
  binary's text and the dyld shared cache (RSS includes resident ones).
* This is what jetsam/memory limits and Activity Monitor's "Memory" column use [Activity Monitor: secondary],
  and what `top -stats mem` prints ("Physical memory footprint of the process", top(1); `libtop.c` reads
  `vm_info.phys_footprint`) [primary].
* Allocator pages that are free-but-dirty stay in `internal` → still counted until the allocator returns them
  with an advice that XNU accounts immediately (see §3).

Ways to read it:
| Tool | What | Root? |
|---|---|---|
| `proc_pid_rusage(pid, RUSAGE_INFO_V4, …)` → `ri_phys_footprint`, `ri_lifetime_max_phys_footprint`, `ri_interval_max_phys_footprint` | in-process or from a helper; exact | no (own uid) [primary header; libc 0.2.191 has v0–v4] |
| `task_info(TASK_VM_INFO)` → `phys_footprint` | in-process | no [primary via top source] |
| `top -l N -s S -pid P -stats pid,mem,…` | sampled | no |
| `footprint <pid>` / `footprint --summary` | categorised breakdown (dirty, swapped, by VM tag) | own process: no [secondary — check `man footprint`] |
| `vmmap --summary <pid>` | regions by tag, malloc zones, dirty/swapped | [secondary] |
| `heap <pid>` | live malloc objects by size/class per zone | [secondary] |
| `leaks <pid>` / `leaks --atExit -- <cmd>` | unreachable malloc blocks; backtraces need `MallocStackLogging` (malloc(3)) | [secondary for leaks; env var primary] |
| `ps -o rss=` | RSS only — do **not** gate on it | |

`ri_*_time` fields of `rusage_info` are reported in Mach absolute time units on Apple silicon (not ns) in many
reports [secondary] — convert with `mach_timebase_info` and verify on the M2.

## 2. System malloc on current macOS [primary: libmalloc @ c49dafa, `doc/xzone_malloc.md`]

* Apple's current general allocator is **xzone malloc** (security-focused, "partly derived from mimalloc").
* "**Deferred reclaim**": instead of synchronous `madvise(MADV_FREE_REUSABLE)`, freed spans are queued in a
  kernel-shared ring; "The kernel monitors process-local and system-wide memory conditions to determine whether
  and when to drain" it. xzone "tell[s] the kernel as eagerly as possible about all of the pages available to be
  reclaimed".
* Thread caches and per-cluster chunks start disabled ("memory-efficient starting configuration") and turn on
  only under contention/volume.
* `malloc_zone_pressure_relief(NULL, 0)` (malloc.h) asks all zones to release memory; returns bytes released.
* Which macOS versions use xzone for third-party binaries by default is **not stated** in the doc → check on the
  M2 with `vmmap`/`heap` zone names [unverified].

Implication: on macOS the **system allocator is a strong default** for a mostly-idle daemon: it cooperates with
the kernel's memory-pressure machinery, has no background thread and pays no wakeups.

## 3. mimalloc and jemalloc on macOS [primary: crate sources]

| | mimalloc (crate `mimalloc` 0.1.52 → `libmimalloc-sys` 0.1.49) | jemalloc (`tikv-jemallocator` 0.7.0 / `tikv-jemalloc-sys` 0.7.1+5.3.1) |
|---|---|---|
| bundled C version | **v3.3.2 by default**, v2.3.2 with feature `v2` | jemalloc 5.3.1-ish |
| return pages on macOS | `MADV_FREE_REUSABLE` ("does immediate rss accounting", mimalloc issue #1097), fallback `MADV_DONTNEED` | `MADV_FREE` (lazy); configure: macho has `default_retain=1` (keeps virtual ranges) |
| purge timing | `purge_delay` default **1000 ms in v3**, 10 ms in v2; purge runs **inside allocator calls** (no background thread) | `dirty_decay_ms` default 10 s, then muzzy decay; purge on allocator activity |
| background purge thread | none | **not available on macOS**: configure enables `JEMALLOC_BACKGROUND_THREAD` only if `abi != macho` |
| forcing a release | `libmimalloc_sys::mi_collect(true)` (needs crate feature `extended`) | mallctl `arena.<i>.purge` / `arena.<i>.decay` (jemalloc(3)); `tikv-jemalloc-ctl` 0.7.0 has no typed wrapper → raw mallctl write [unverified usage] |

Consequence for an idle daemon: after a burst, mimalloc/jemalloc on macOS **keep the freed dirty pages until the
next allocation activity** (no thread runs the decay clock). A daemon that goes quiet right after a burst can sit
on its peak footprint for hours. Mitigation if you adopt one: call the explicit collect/purge on the
"entering idle" transition and on memory-pressure WARN. jemalloc's `MADV_FREE` pages may stay in
`phys_footprint` until the kernel reclaims them [unverified — measure].

## 4. Decision

1. Start with the **system allocator** (no `#[global_allocator]`).
2. Only switch if a measured problem exists (allocation-heavy CPU hot path, or fragmentation growth in the
   24 h soak). Candidate: mimalloc v3, with `mi_collect(true)` on idle/pressure.
3. The switch must pass the same gate: idle footprint after burst (§5), 24 h slope, wakeups unchanged.
4. Never ship `dhat::Alloc` or `MallocStackLogging` in production builds.

## 5. Allocator A/B protocol (run on the M2)

1. Start daemon, warm 2 min, record `phys_footprint` F0.
2. Inject a reproducible burst (e.g. replay 10k protocol messages / 200 MB transient buffers).
3. Record peak Fp, then let it idle; sample footprint every 10 s for 10 min → F(t).
4. Report: Fp − F0, F(60 s) − F0, F(600 s) − F0, CPU-s for the burst, wakeups during idle.
5. Repeat 3x per allocator (system / mimalloc v3 / mimalloc v3 + `mi_collect` on idle). Pick the lowest
   F(600 s) whose CPU cost is within 10 % of the best.
6. Also run `memory_pressure -S -l warn` (system_cmds; simulates pressure) during idle and confirm the daemon's
   pressure handler fires and footprint drops.

## 6. Fragmentation over days

* Long-lived + short-lived allocations interleaved = fragmentation. Keep long-lived state allocated once at
  startup (pre-size `Vec`/`HashMap`), reuse buffers (`BytesMut` pools of fixed size classes), cap caches by
  bytes, not entries.
* Watch `ri_lifetime_max_phys_footprint` vs current: a growing gap after repeated bursts that never closes =
  fragmentation or a cache without a cap. A monotonic rise of the *idle floor* across days = leak → `leaks`,
  `heap`, DHAT.
* DHAT (`dhat` 0.3.3): heap profiling + "heap usage testing" (`dhat::HeapStats::get()` + `dhat::assert!`) — use
  in CI unit tests to pin allocations of hot paths. Crate self-describes as experimental and low-maintenance.
  Release builds only (debug too slow).

## 7. Memory-pressure behaviour (macOS)

`DISPATCH_SOURCE_TYPE_MEMORYPRESSURE` with `DISPATCH_MEMORYPRESSURE_NORMAL|WARN|CRITICAL` (libdispatch
`source.h`; exposed in `dispatch2` 0.3.1). Apple's header guidance [primary]: react by "changing their future
memory use behavior, e.g. by reducing cache sizes of newly initiated operations"; "applications should NOT
traverse and discard existing caches for past operations when the system memory pressure enters an elevated
state, as that is likely to trigger VM operations that will further aggravate system memory pressure."
So: on WARN → lower cache caps for new entries, stop prefetching, defer non-urgent batch jobs, call
`malloc_zone_pressure_relief`/allocator collect once; on CRITICAL → additionally drop optional work and
reject new large requests; on NORMAL → restore. Test with `memory_pressure -S -l warn|critical`.
