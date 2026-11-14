# Release profile, binary size, startup, dependency bloat

Source: Cargo reference `doc/book/src/reference/profiles.md` (rust-lang/cargo master, Nov 2026) [primary];
perf-book (nnethercote) build-configuration chapter [secondary]; cargo-pgo README @ 2f33b93 [primary for the tool].

## 1. Defaults you are changing (Cargo `release`)

`opt-level=3, debug=false, strip="none", lto=false (thin-local), panic="unwind", incremental=false,
codegen-units=16, split-debuginfo` platform-specific (`unpacked` on macOS when debug info is on).

## 2. Recommended profile for fogar-agent (measure before/after)

```toml
[profile.release]
opt-level = 3            # idle cost is dominated by wakeups, not codegen; keep 3 unless size matters more
lto = "fat"              # whole-program; smaller + faster; slower link. "thin" if CI time hurts
codegen-units = 1        # better optimisation, slower build
panic = "abort"          # see trade-off below
strip = "symbols"        # ship without symbols...
debug = "line-tables-only"  # ...but archive a dSYM:
split-debuginfo = "packed"  # macOS: produces .dSYM next to the binary; keep it per release for symbolication
incremental = false

[profile.profiling]      # for samply / Instruments / xctrace: same codegen, symbols kept
inherits = "release"
strip = "none"
debug = true
```
Notes:
* `strip="symbols"` + `split-debuginfo="packed"` keeps crash symbolication possible from the archived dSYM; Cargo
  docs show `debug=true, split-debuginfo="packed", strip="debuginfo"` (+ `trim-paths="object"`) to keep absolute
  object paths out of the executable on macOS. Verify the dSYM is produced with your toolchain.
* **panic="abort" trade-off:** smaller binary, no unwinding tables, but a panic in any tokio task kills the
  whole daemon (with "unwind", tokio catches task panics into the `JoinHandle`). With launchd `KeepAlive` that is
  a restart (cold start cost + reconnect storm risk). Choose consciously; if "abort", the restart path must be
  cheap and rate-limited. Tests/benches ignore the setting.
* `-C target-cpu`: for a binary that only runs on Apple silicon, `aarch64-apple-darwin` already targets Apple
  cores generically; `target-cpu=native` makes CI artifacts non-portable — avoid unless measured benefit.

## 3. PGO / BOLT

* **PGO works on macOS** in principle (rustc `-Cprofile-generate/-Cprofile-use` + `llvm-profdata` from the
  `llvm-tools` rustup component; `cargo-pgo` automates it). Value for an idle daemon is low: PGO speeds up hot
  code, it does not remove wakeups. Consider only for a measured CPU-bound slice (e.g. parsing/codec). [primary
  for tool existence; benefit unverified]
* **BOLT:** cargo-pgo calls BOLT support "experimental" and recommends Docker; LLVM BOLT targets ELF binaries.
  Treat BOLT as **not applicable to Mach-O/macOS** [secondary — confirm in llvm-project/bolt docs before relying].

## 4. Binary size and startup cost

* What matters for a long-running daemon: (a) resident pages actually touched (mostly not counted in
  `phys_footprint` because text pages are clean file-backed), (b) restart cost if it crashes or is relaunched
  (launchd, updates), (c) disk/download size.
* Measure: `ls -l`, `size -m` / `otool -l` (sections) [secondary], `cargo bloat --release --crates` (third-party
  tool; unverified version), cold start = time from spawn to "ready" log line, measured 10x.
* Startup should do no eager work that can be lazy: no full config scans, no prefetching, connect once; heavy
  init (indexes, caches) on a BACKGROUND-QoS thread after "ready".

## 5. Dependency bloat rules

1. Every new dependency in a slice must state: why, size impact (`cargo bloat`/binary diff), threads it spawns,
   timers it arms, allocator it pulls in.
2. Reject crates that spawn hidden threads or periodic timers (metrics exporters with push intervals, file
   watchers that poll, HTTP clients with connection-pool reapers on short intervals) unless configurable to
   event-driven / coarse intervals.
3. `default-features = false` everywhere, enable only needed features (`tokio` especially: pick
   `rt`, `rt-multi-thread` only if used, `net`, `time`, `sync`, `macros`, `io-util`, `signal` as needed; not `full`).
4. One TLS stack, one async runtime, one HTTP stack. `cargo tree -d` must show no duplicate major versions of
   heavy crates (tokio, hyper, rustls/openssl, regex).
5. Logging: `tracing` with a non-blocking writer; no per-event `format!` at INFO in hot paths; ensure the writer
   thread blocks (no polling flush timer) [unverified for specific appenders — check their source].
