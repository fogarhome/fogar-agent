# ADR-0003: `panic = "abort"` in release + synchronous crash record + crash-loop breaker

Status: **PROVISIONAL**

## Decision
- Release profile `panic = "abort"`; the supervisor (launchd `KeepAlive`, systemd `Restart=`)
  restarts the process. A panic is a broken invariant; restart-from-disk is the one recovery path.
- `fogar-agentd` installs a panic hook first thing in `run`; it writes a content-free record
  (message, location, thread, uptime, backtrace) to `<state>/crash/last.txt` and fsyncs it
  (capped at 1 MiB).
- Boot ledger `<state>/boot.ledger` (`start|clean|stable|safe`; current run only, <= 64 lines). A *run* is the sequence
  of crashes (unclean starts) since the last `clean`/`stable`. 4 crashes of a run inside 10 min
  => SafeMode: delayed start 60 s, doubling with every further crash of the run up to 32 min,
  cancellable by SIGTERM; a `clean` or `stable` boot ends the run. Policy is pure
  (`fogar_core::crashloop`), I/O in the composition root. (Review code#7a: the first version
  counted crashes only inside the 10-min window, so the escalating delays pushed old crashes out
  of the window and the delay plateaued at 4-8 min; the 3 h retention covers a full escalation,
  60+120+...+1920 s = 62 min. Test: `a_sustained_crash_loop_reaches_the_maximum_delay`.)
  Round-2 review F4: that still made a sawtooth (60 s -> 1920 s, then 0 s for 4 boots once the
  fast first crashes aged past 3 h). The trip is now persisted: a SafeMode boot writes a `safe`
  line after its `start`, and a `safe` line in the run keeps it tripped. The run ends only on
  `clean`/`stable` or a quiet gap > 3 h (between entries or since the last one: machine off).
  The file holds the current run only, at most 64 lines (`crashloop::retain`). Test:
  `forty_boot_crash_loop_never_drops_back_to_zero` (40-boot simulation incl. pruning).
- What counts as clean (review code#2/code#7b): SIGTERM/SIGINT handlers are installed before the
  ledger records `start`; any stop asked for by a signal writes `clean`, even if a shutdown phase
  overran its deadline (the overrun is logged separately, not counted as a crash). Shutdown
  budget 4 s: ingress 1/4, egress (WebSocket close, `close_timeout` 1 s) the rest minus 1/4, ops
  1/4.
- The state dir is validated before the ledger is touched (ADR-0006 item 10): owner, 0700, not a
  symlink, no `/tmp` fallback; ledger/crash files are opened `O_NOFOLLOW`, temp files
  `create_new`. RLIMIT_CORE = 0 at daemon start (the core would contain the client key).
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
