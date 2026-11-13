# ADR-0006: Skill conflicts and slice-0 deviations

Status: **PROVISIONAL** (each item names what retires it)

1. **Protocol types are `Cow<'a, str>`** instead of RUST-MODEL's `&'a str`, and server messages
   also derive `Serialize`: the mock gateway decodes client frames and encodes server frames with
   the same types. Encoding still borrows. (Retire: never; this is the intended shape.)
2. **Audio port shape (fogar-realtime-audio vs slice 0).** The skill's port hands an `RtSink`
   (rtrb ring) to the adapter. Slice 0 uses a bounded `EventStream<AudioChunk>` (16 kHz mono f32
   chunks, drop-newest-and-count) because no real capture exists yet. `EventSink::push` uses tokio
   `try_send`, which wakes a Waker: forbidden on a CoreAudio RT thread. The S2 adapter must push
   from its dsp thread; S2's contract PR may switch the port to the RtSink shape. Captures not at
   16 kHz mono are refused (no resampler yet).
3. **Clock suspend semantics** are declared per adapter (`SuspendBehavior`); `TokioClock` is
   `ExcludesSuspend`. No wall-vs-monotonic jump detector yet (S1).
4. **NET-1 (wss only) vs the gateway reality (plain ws on :8004 behind a proxy).** Slice 0 has no
   TLS: `WsConfig` refuses `wss://` with an explicit error and accepts `ws://` only for loopback
   hosts, so the client key never crosses a network in clear text. S1 adds rustls
   (platform verifier) and decides how a tailnet `ws://` (100.64.0.0/10) may be allowed.
   Review sec#3 (accepted risk, slice 0 only): on loopback the key is still readable by any local
   process that can listen on / proxy that port (e.g. a squatter on :8004 when the gateway is
   down). The daemon prints a loud `WARNING: ... UNENCRYPTED ws://` line at every start
   (test `stop_ledger::plain_ws_gateway_prints_a_loud_warning`). TLS is NOT implemented now;
   retire with S1.
5. **Energy vs watchdog cadence**: heartbeat 10 s / stale 60 s (energy skill: <= 0.5 wakeups/s)
   instead of longrun's 1 s / 5 s. See ADR-0003.
6. **Runtime**: `current_thread` tokio runtime on a dedicated `fogar-rt` thread (energy skill),
   main thread only waits — it becomes the CFRunLoop thread on macOS when Apple frameworks need it
   (fogar-macos-daemon §1). Thread QoS UTILITY applied via the `PowerPolicy` port (UNTESTED).
7. **No half-open detection in S0**: the session only sends `keepalive`; WS-level pings with an
   inbound idle deadline arrive with S1 ("real WS client robustness").
8. **Loom cfg is `fogar_loom`, not `loom`**: `--cfg loom` would also switch tokio's internals.
9. **Toolchain pin vs offline sandbox**: `rust-toolchain.toml` pins 1.97.0; the build sandbox
   could not download it, so `scripts/gates.sh` accepts an installed `stable` only if it is
   exactly 1.97.0 and says so in the summary.
10. **`.claude/` not written by the build session**: writing it required an approval nobody gave;
    the reviewed agent-team config is staged in `agent-team/` and installed with
    `scripts/install_agent_team.sh`.

## To verify on the M2
Items 2, 3 and 6 are macOS behaviour: verify in S2 (audio), S1 (sleep/wake) and S7 (energy gate).
10. **State directory (review sec#1)**: no predictable fallback when `HOME` is unset (the daemon
    refuses to start and asks for `--state-dir`); the state dir is validated (absolute, owner =
    getuid, real dir, 0700) before use; ledger/crash files `O_NOFOLLOW`, temp files `create_new`.
    The composition root uses `rustix` (safe wrappers) for getuid / O_NOFOLLOW / RLIMIT_CORE, so
    it stays `#![forbid(unsafe_code)]`. macOS: same code path, UNTESTED until macOS CI.
11. **Inbound queue byte cap (review sec#4)**: `queues::INBOUND_BYTES` = 4 MiB in addition to
    the 128-message cap; `decode_server` is two-pass (tag, then variant struct) instead of
    serde's buffering internally-tagged enum; unused GUESSED fields (`tool_call.args`,
    `pipeline_event` extras) are skipped, not kept. `ready` is decoded leniently (review code#5).
12. **Capture consent (review sec#8)**: not in slice 0 (no capture exists). Hard requirement for
    S2 in docs/ROADMAP.md: one choke point in core through which every capture start passes,
    checking local consent state; gateway messages can never start capture by themselves.
