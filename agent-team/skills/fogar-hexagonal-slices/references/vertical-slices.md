# Vertical slices in a hexagonal workspace (+ parallel AI agents)

## 1. What a slice is here

A slice is one user-visible capability cut through **every ring that it needs**:

```
UI (menu bar app / CLI)  <-IPC->  composition root  ->  core use case  ->  port(s)  ->  adapter(s)  -> OS
                                         |                    |
                                     fogar-proto  <------  encode/decode (gateway wire)
```

It is *done* only when it runs end to end on the real target OS **and** in-memory with fakes.
Never build "all ports first" or "all of proto first".

## 2. Slice 0: walking skeleton (first week)

Smallest thing that exercises every crate, CI job and rule:
- proto: one message (`Status`) with encode/decode + round-trip + truncation tests.
- ports: `SystemProbe` (load only), `Uplink`, `Clock`.
- core: `Governor::tick` (decide mode -> encode -> send), pure `decide()`.
- testkit: fakes + contract suites for those three ports.
- host: `TokioClock`, `StderrUplink` (logs frames; replaced later without core change).
- agent-macos: `MacSystemProbe` (getloadavg only, everything else `Unsupported`), `main.rs`
  composition root, `tests/contract.rs`.
- archtest R1-R6, clippy disallowed lists, cargo-deny, CI matrix (Linux + macOS).
- IPC/UI: a `status` command over the IPC port returning current mode (or postpone to Slice 1,
  but the IPC port + fake should exist in Slice 0 if the UI team starts in parallel).
This is exactly `references/example-workspace` (minus IPC).

## 3. Suggested slice order for fogar-agent (each crosses all rings)

1. Skeleton: probe -> governor -> uplink(stderr) -> status via IPC.
2. Real gateway uplink (fogar-proto real messages; reconnect/backoff via Clock; secrets port for token).
3. Autostart + lifecycle (launchd LaunchAgent, single instance, SIGTERM drain) + power policy QoS.
4. Thermal/battery/idle push events on macOS (NSProcessInfo notifications) -> governor reacts.
5. Audio source capture slice (permission flow end-to-end incl. UI prompt) -> encoded upload.
6. Screen capture slice (TCC permission, latest-frame stream) -> upload.
7. Soak/energy slice: 72 h run budget checks (CPU %, RSS, wakeups) as a release gate.
Windows/Linux: each later "port" of the agent = new adapter crate passing the existing contract
suites; no core change expected (if core changes, the port leaked).

## 4. Slice template (copy into the slice's issue/PR description)

```
Slice: <name>                      Owner(s): <crate -> agent/human>
User-visible outcome: <one sentence, observable on a real Mac>
Rings touched: proto[ ] ports[ ] core[ ] testkit[ ] host[ ] agent-macos[ ] ipc/ui[ ]

Contract changes (merge FIRST, separate PR):
  - port trait diff:            <...>   breaking? <no: defaulted method | yes: list adapters>
  - capability flags added:     <...>   default Unsupported
  - contract-suite clauses:     <...>
  - fake updated:               <...>
  - proto messages (wire):      <...>   version bump? <...>

Core behaviour (pure functions first):
  - decision rules + unit tests table
Adapters:
  - macOS: <APIs used, permissions, threads/run loops, energy notes>
  - others: Unsupported (explicit) | Emulated (how)
Done when: see DoD checklist (all boxes)
Out of scope: <...>
```

## 5. Definition of Done for a slice

- [ ] Acceptance test in `fogar-core/tests/<slice>.rs` using fakes passes on Linux CI (fast, deterministic, virtual time).
- [ ] Every adapter touched passes the **unchanged-or-extended** contract suite on its own OS runner;
      the fake passes the same suite.
- [ ] Adapters that don't support it report `Unsupported` (contract-checked), core degrades gracefully (test with `FakeSystemProbe::minimal()`-style fake).
- [ ] Runs end-to-end on a real Mac via the composition root (manual or scripted smoke test, recorded in PR).
- [ ] Energy/resource budget for the new code path stated and measured (wakeups/min, CPU %, RSS delta);
      polling intervals adaptive; no busy loops; timers coalesced where the OS allows.
- [ ] Cancellation: new async paths are cancel-safe or documented; shutdown path tested.
- [ ] Errors mapped to port error categories; no panics on OS errors; logs carry source detail.
- [ ] archtest, clippy (incl. disallowed-*), cargo-deny, `cargo fmt` green; no new `cfg(target_os)` outside adapters.
- [ ] Wire changes: proto round-trip + truncation/fuzz-ish tests; version/compat note.
- [ ] Docs: port rustdoc states semantics (units, threading, cancel-safety, cost budget); ARCHITECTURE.md invariant added if a new rule appeared (rust-analyzer style "**Architecture Invariant:**" notes [primary: rust-analyzer docs/book/src/contributing/architecture.md]).

## 6. Parallel AI-agent development

Ownership (CODEOWNERS-style, one owner per crate per slice):
| Crate | Owner | Others may |
|---|---|---|
| fogar-ports + fogar-testkit (contracts, fakes) | "contract owner" (human or lead agent) | propose via PR only |
| fogar-proto | protocol owner | read |
| fogar-core | core agent | read ports |
| fogar-agent-macos | macOS adapter agent | — |
| fogar-host | host agent | — |
| fogar-archtest, deny.toml, clippy.toml, CI | architecture owner (human) | never edit to make a check pass |

Workflow (contract-first):
1. Contract PR: trait + types + capability flags + fake + contract suite + a failing-or-ignored
   core acceptance test. Small, reviewed by a human, merged to main first.
2. Fan out worktrees from that commit: core agent (makes acceptance test pass with fakes), adapter
   agent(s) (make contract suite pass on their OS), UI/IPC agent (against IPC fake).
3. Integrate: composition-root wiring PR + real-Mac smoke test.

Rules that keep parallel worktrees from colliding:
- An agent edits only the crates it owns; touching another crate = stop and open a contract PR.
- Dependency versions only via `[workspace.dependencies]`; one agent (architecture owner) adds new
  third-party deps, so `Cargo.lock` and `deny.toml` conflicts stay rare. Expect `Cargo.lock` merge
  conflicts anyway; resolve by re-running `cargo update -p <crate>` / `cargo generate-lockfile`,
  never by hand.
- Each worktree uses its own target dir (Cargo holds a lock on the build directory; concurrent
  builds in one dir serialize).
- Agents run before pushing: `cargo fmt --check && cargo clippy --workspace --all-targets && cargo test -p <owned crates> -p fogar-archtest`.
- "Agent may not modify" list: archtest rules, clippy.toml, deny.toml, contract suites (only the
  contract owner). An agent that needs a rule change must explain why in the PR.
- Prompts to adapter agents include: the port rustdoc, the contract suite file, the platform
  capability table, energy budget, and "report Unsupported rather than emulate unless asked".
- On a Linux worktree an agent cannot compile macOS-gated code; require macOS CI green before merge.

## 7. YAGNI vs "hexagonal everywhere" (pragmatic rule)

The user wants ports for platform unknowns up front. Rule:
- **Port it now** if (a) it touches the OS / outside world (all 8 planned ports qualify), or (b) it is
  a source of nondeterminism you need to fake in tests (time, randomness, IDs, network).
- **Do not port** pure in-process logic (codecs, policies, state machines, buffers): plain types/functions in core/proto.
- **Do not generalise a port beyond what one real adapter + one fake need.** Design the macOS adapter
  first; write the port from the core's need; add `Support`/capability fields only when a known
  platform difference exists (documented in the capability table), not speculatively.
- **Rule of two for shared adapter code**: extract into `fogar-host` only when a second adapter
  crate needs the same code.
- **Second-platform check**: before freezing a port, write a 10-line "how would Windows/Linux do this?"
  note (API names, push vs poll, permission model). If an answer forces a different shape, adjust now;
  it is cheap. If unknown, mark the capability `Unsupported` and move on.
