# ADR-0003: `panic = "abort"` in release + synchronous crash record + crash-loop breaker

Status: **PROVISIONAL**

## Decision
- Release profile `panic = "abort"`; the supervisor (launchd `KeepAlive`, systemd `Restart=`)
  restarts the process. A panic is a broken invariant; restart-from-disk is the one recovery path.
- `fogar-agentd` installs a panic hook first thing in `run`; it writes a content-free record
  (message, location, thread, uptime, backtrace) to `<state>/crash/last.txt` and fsyncs it
  (capped at 1 MiB).
- Boot ledger `<state>/boot.ledger` (`start|clean|stable`, bounded to a 10-minute window).
  4+ unclean starts in 10 min => SafeMode: delayed start 60 s doubling to 32 min, cancellable by
  SIGTERM. Policy is pure (`fogar_core::crashloop`), I/O in the composition root.
- Config/auth problems never exit (exiting makes launchd loop): the link parks and reports health.
- An OS-thread watchdog aborts (with a crash record) if the runtime heartbeat is > 60 s stale.
- Exit code after SIGTERM is always 0 (a non-zero exit would make the supervisor restart a
  process that was asked to stop); the ledger records whether the stop was clean.

## Conflict between skills (resolved here)
fogar-longrun-reliability: 4 crashes / 10 min; fogar-macos-daemon: >= 5 starts / 10 min.
Chosen: 4 unclean starts (crashes), because a clean restart (update, user stop) must not count.

## Slice 0 state
SafeMode only delays the start; "optional slices disabled" has nothing to disable yet.
Watchdog period 10 s / threshold 60 s (energy: 0.1 wakeups/s), not the skill's 1 s / 5 s.

## To verify on the M2
- `KeepAlive{SuccessfulExit=false}` restarts after SIGABRT; measured ThrottleInterval.
- The crash record survives an abort (fsync) and is readable on next boot.
- Watchdog + heartbeat add <= 0.2 wakeups/s idle.
