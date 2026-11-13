# Observability at near-zero cost; leak and growth detection over days

## Always-on probes (sampled every 60 s by the `ops` task)

| Probe | How | Cost |
|---|---|---|
| Alive tasks | `Handle::current().metrics().num_alive_tasks()` (stable) | atomic load |
| Global queue depth, worker busy time | `global_queue_depth()`, `worker_total_busy_duration(i)`, `worker_park_count(i)` (stable) [primary, `runtime/metrics/runtime.rs`] | atomic loads |
| Blocking threads and queue | `num_blocking_threads()`, `blocking_queue_depth()`: **need `tokio_unstable`** [primary] | — |
| Channel depth | `Receiver::len()`/`Sender::capacity()` per named queue | cheap |
| Open fds | Linux: count `/proc/self/fd` [exp]. macOS: `proc_pidinfo(PROC_PIDLISTFDS)` or count `/dev/fd` [unverified on macOS] | syscall per minute |
| Memory | Linux: `/proc/self/statm` [exp]. macOS: `task_info(TASK_VM_INFO).phys_footprint` (what Activity Monitor shows) [secondary]. Allocator: jemalloc `epoch::advance()` then `stats::allocated::read()` / `stats::resident::read()` (tikv-jemalloc-ctl 0.7.0, feature `stats`) [exp E8: allocated=735800 resident=2523136] | µs |
| SQLite | WAL file size, `page_count`/`freelist_count`, outbox rows | a stat() call and a cheap query |
| Logging | `ErrorCounter::dropped_lines()` from tracing-appender `NonBlocking::error_counter()` [primary, exp] | atomic |
| Event-loop lag | heartbeat task measuring tick lateness (E3) | trivial |

**Alerting on slope**: keep a 24 h ring of samples (1440 × a small struct, bounded). Compute a least-squares slope per hour after a 1 h warm-up. Flag `tasks_slope > 0` while the session count is constant, `rss_slope > 1 %/h`, `fds_slope > 0`, or a monotonically growing queue `len`. In E5 the planted leak was obvious within 30 s from `alive_tasks` (3 → 21), long before RSS looked alarming (3.3 → 5.1 MiB) [exp, `examples/soak-*.csv`].

## Heap profiling (dev and soak, not prod)

- **dhat** 0.3.3 (2024-02-04, stable but old) [primary: crates.io]: `#[global_allocator] static A: dhat::Alloc = dhat::Alloc;` plus a `dhat::Profiler` in a soak binary. It reports peak, total and retained-at-exit by call site, and has a testing mode for asserting heap stats [secondary]. Expect a large slowdown, so use it in compressed soaks, not in a 72 h run.
- **jemalloc** (tikv-jemallocator 0.7.0) gives cheap in-process stats in production. Its heap profiling (`prof` features) is heavier and Linux-oriented [secondary]. macOS default allocator vs jemalloc: measure fragmentation over a 72 h soak before committing [open question].
- macOS tools: `leaks <pid>`, `heap <pid>`, `vmmap --summary <pid>`, `footprint <pid>`, and Instruments Allocations [secondary]. Script `footprint` every 10 min during nightly soaks.

## tokio-console and tokio-metrics cost

- console-subscriber 0.5.0 needs `--cfg tokio_unstable` and tokio's `tracing` feature. It records every task's spawn and poll events and keeps them for a retention window, which adds per-poll overhead and memory. **Dev and soak only** [secondary].
- tokio-metrics 0.5.2 `TaskMonitor::instrument(fut)` adds a few atomics per poll [secondary]. It is fine for selected long-lived tasks in production. `RuntimeMonitor::intervals()` gives `busy_ratio()` and `mean_polls_per_park()`. Unstable metrics need `tokio_unstable` [primary, README].
- Do **not** build production with `tokio_unstable` just for metrics. The stable set above covers the leak and stall signals.

## Logging/tracing configuration (compiled: `examples/src/bin/e8_snippets.rs`)

```rust
let appender = tracing_appender::rolling::Builder::new()
    .rotation(Rotation::DAILY).filename_prefix("fogar-agent").filename_suffix("log")
    .max_log_files(7).build(dir)?;
let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
    .buffered_lines_limit(8_192).lossy(true).finish(appender);
let dropped = writer.error_counter();
let (filter, reload_handle) = reload::Layer::new(EnvFilter::new("info"));
tracing_subscriber::registry().with(filter).with(fmt::layer().with_writer(writer).with_ansi(false)).init();
```
Facts [primary, tracing-appender 0.2.5]: the default `DEFAULT_BUFFERED_LINES_LIMIT` is 128,000 and the default is lossy ("drop spans/events at capacity"). Non-lossy "will block their respective threads" and is **forbidden** here. Rotation is **time-based only** (MINUTELY, HOURLY, DAILY, WEEKLY, NEVER) and the time is `OffsetDateTime::now_utc()`. **There is no size cap**, so total size ≈ `max_log_files × daily volume`. Enforce volume with:
- a per-callsite rate limit for repeated errors (log the first, then summarize "n more in 60 s"), so a reconnect storm cannot write gigabytes;
- an `info` default level, with `debug` enabled only temporarily via `reload_handle.reload(...)` (config reload or a gateway command) and auto-reverted after N minutes;
- no per-frame, per-audio-chunk or per-row logs. Use counters instead;
- an hourly check of the log directory size by the ops task: if it exceeds a cap (for example 200 MiB), delete the oldest files and raise the filter level.
If a hard size cap is required, `file-rotate` 0.8.0 or `logroller` 0.1.12 are candidates [unverified].

Keep the `WorkerGuard` alive in `main` and drop it as the last shutdown step. With `panic=abort` its buffer is lost, which is why the panic hook writes synchronously.

Sampling: for high-rate spans, sample 1-in-N with a custom `Filter`, or emit aggregate events from counters every minute. Prefer `#[instrument(skip_all, fields(id))]` on per-session spans. Never put secrets in fields: `SecretString` prints `[REDACTED]` [exp].
