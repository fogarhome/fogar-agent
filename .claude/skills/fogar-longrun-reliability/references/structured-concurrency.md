# Structured concurrency with tokio 1.54 / tokio-util 0.7.20

Labels: [primary] = read in the crate source or official docs; [exp] = measured here (Linux x86_64, rustc 1.97); [secondary] = reputable writing; [unverified] = belief.

## Ownership model

```
main
 └─ Supervisor (root CancellationToken, TaskTracker per shutdown phase)
     ├─ data_plane: SessionEngine, SQLite writer bridge, outbox pump
     ├─ io_plane:   GatewayConnection supervisor ── per-session JoinSet (reader, chunk uploader)
     ├─ edge:       WakeWord bridge (OS thread + channel), SSH sessions (JoinSet, max N)
     └─ ops:        health/metrics sampler, config watcher, clock-jump detector
```

- **TaskTracker vs JoinSet** [primary, tokio-util `task_tracker.rs` docs]: a `JoinSet` "keeps track of the return value of every inserted task", so if you keep inserting without calling `join_next`, "their return values will keep building up and consuming memory". `TaskTracker` frees each task as soon as it exits, is `Clone`, does not need `&mut` to spawn, and `wait()` returns only once it is **closed and empty**. "Dropping a `TaskTracker` does not abort the tasks." Dropping a `JoinSet` aborts all of its tasks [primary, `join_set.rs`].
  - Use **TaskTracker** for long-lived services and for fire-and-track work (one per shutdown phase).
  - Use **JoinSet** for a bounded group whose results you consume, or which must die together (per-session helpers, parallel uploads). Always drain it in the same `select!` loop: `Some(res) = set.join_next(), if !set.is_empty() => handle(res)`.
  - Use **`AbortOnDropHandle`** (tokio-util, `rt` feature) for a single child that must die with its parent struct.
- **CancellationToken** [primary]: `child_token()` (cancelling the parent cancels the children, not the reverse), `cancelled()` / `cancelled_owned()`, `run_until_cancelled(fut)`, `drop_guard()`. `FutureExt::with_cancellation_token` and `timeout` exist on `tokio_util::future::FutureExt`.
- Tokio's shutdown guide: (1) decide when to shut down, (2) tell everyone (CancellationToken), (3) wait (TaskTracker) [primary, tokio.rs topics/shutdown].

## Phased shutdown (E2, executed)

See `examples/src/bin/e2_shutdown.rs`. Measured output [exp]:
```
[  201 ms] SIGTERM received -> phase 1: cancel ingress, drain writer
[  207 ms] producer stopped after 34 msgs
[  259 ms] writer drained 34 msgs and flushed
[  259 ms] phase 2: cancel egress (close sockets)
[  280 ms] connection closed cleanly
[  280 ms] all phases done
[  780 ms] shutdown_timeout(500ms) returned; process exits (blocking thread leaked)
```
Pattern: the writer has **no token**. It exits when every `Sender` is dropped, which guarantees a full drain. Producers watch the ingress token. A spawn_blocking job that ignores cancellation would hold the runtime drop for 30 s; `Runtime::shutdown_timeout` bounds the wait [primary: "runtime shutdown will wait indefinitely for all started spawn_blocking to finish running"]. For the signal, use `tokio::signal::unix::signal(SignalKind::terminate())` (launchd/systemd send SIGTERM) plus `ctrl_c()` in dev. On Windows, the SCM stop control comes through the service adapter.

## select! cancellation safety

Tokio's definition [primary, `macros/select.rs`]: a future is cancel-safe if dropping it before completion and recreating it is a no-op. The docs list as **safe**: `mpsc::Receiver::recv`, `UnboundedReceiver::recv`, `broadcast::Receiver::recv`, `watch::Receiver::changed`, `TcpListener::accept`, `AsyncReadExt::read`/`read_buf`, `AsyncWriteExt::write`/`write_buf`, and `StreamExt::next`. **Unsafe (data loss)**: `read_exact`, `read_to_end`, `read_to_string`, `write_all`. **Unsafe (lose your queue position)**: `Mutex::lock`, `RwLock::read`/`write`, `Semaphore::acquire`, `Notify::notified`. `mpsc::Sender::send`: if another branch wins, "the message is dropped and will be lost. To avoid losing messages, use `reserve`" [primary, `mpsc/bounded.rs`].

E1 [exp] (`examples/src/bin/e1_cancel_safety.rs`) pops 2000 items from a local queue and sends each in a `select!` against a 300 µs tick, with capacity 4 and a slow consumer:
```
send() in select!   : delivered 387/2000, tick wins 1613, LOST 1613
reserve() in select!: delivered 2000/2000, tick wins 8181, LOST 0
```
Rule: **take the item only after the permit exists.**

Other select! rules:
- Use `biased;` when the cancel branch must win deterministically, and make sure no branch can starve the others (a hot `recv` above a tick). Tokio's default random branch order is fair.
- Disable exhausted branches with `, if cond` guards, so a closed `recv` returning `None` does not spin.
- Long work inside a branch body blocks every other branch, including the heartbeat. Bound it with timeouts, or move it to a task.
- A `Sleep` created inside the loop is reset on every iteration. For a deadline that spans iterations, pin it outside the loop (`tokio::pin!(sleep_until(deadline))`) or use an `interval`.

## spawn_blocking pitfalls

[primary, `task/blocking.rs` and `runtime/builder.rs`]
- The default `max_blocking_threads` is **512**. Beyond that, "tasks are put in a queue" (unbounded). The idle keep-alive is 10 s.
- Blocking tasks "cannot be aborted because they are not async". `abort()` only prevents a task that has not started yet.
- Use a **Semaphore** to limit parallel CPU-heavy work ("a semaphore or some other synchronization primitive should be used").
- E3 [exp], with `max_blocking_threads(2)` and three 500 ms jobs: the third job waited 500 ms in the queue. With 512 threads the queue is hidden until the machine thrashes. For fogar: **never** use spawn_blocking for long-lived loops (SQLite writer, audio, SSH PTY readers). Give those a dedicated `std::thread` with a bounded channel and an explicit join on shutdown.

## Detecting a blocked or starved executor

E3 [exp] (`examples/src/bin/e3_stall_detect.rs`) blocks both workers with `std::thread::sleep(1s)`:
```
[  800 ms] OS watchdog: runtime heartbeat stale for 499 ms   <- detected DURING the stall
...
[ 1301 ms] in-runtime probe: tick lag 950ms                  <- only AFTER the stall ends
```
- **Production**: a heartbeat task plus an **OS-thread watchdog**, which costs about one atomic store per second. Escalation: log, then a metrics snapshot, then a crash record plus `abort()` after N seconds (launchd restarts the process).
- **Development**: `tokio-metrics` `TaskMonitor` gives `slow_poll_ratio`, `mean_slow_poll_duration` and `long_delay_ratio` [primary, tokio-metrics 0.5.2 docs]. `RuntimeMonitor` gives `busy_ratio()`. Poll-time histograms and `on_before_task_poll`/`on_after_task_poll` hooks require `--cfg tokio_unstable` [primary, `builder.rs`]. The task dump (`Handle::dump`) requires `tokio_unstable` and the `taskdump` feature, and is **Linux-only** [primary, `macros/cfg.rs`], so it is not available on macOS.
- Clippy: enable `clippy::await_holding_lock` and an internal deny of `std::thread::sleep` in async modules, and review every `Mutex` held across `.await`.

## Panics inside tasks (unwind builds)

`JoinError::is_panic()`/`into_panic()` exist [primary]. A detached task's panic is reported **only** by the panic hook (E3). The tokio `UnhandledPanic::ShutdownRuntime` builder option is **unstable** (`tokio_unstable`) [primary]. The tokio `Mutex` does **not** poison, so after a panic the guarded data can be half-updated without any signal [primary, `sync/mutex.rs`]. This is part of the reason for `panic = "abort"` (see panic-supervision.md).
