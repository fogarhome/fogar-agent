# Fakes, contract suites, and long-running behaviour tests

## 1. Test pyramid for a hexagonal daemon

| Layer | Where | Runs on | Speed |
|---|---|---|---|
| Pure functions (decide, codec, state machines) | crate unit tests | any host | µs |
| Core use cases over fakes (slice acceptance tests, virtual-time soak) | `fogar-core/tests/` | any host | ms |
| Contract suite vs **fake** | `fogar-testkit/tests/fakes_pass_contracts.rs` | any host | ms |
| Contract suite vs **real adapter** | `fogar-agent-<os>/tests/contract.rs` | that OS runner | ms-s |
| Composition-root smoke / soak | `fogar-agent-<os>` bin, scripted | real machine | min-days |
| Architecture fitness | `fogar-archtest` | any host | <1 s |

## 2. Contract suite pattern (compile-checked in example-workspace)

- Lives in `fogar-testkit::contract::<port>` as **plain functions taking `&dyn Port`** (sync,
  dyn ports) or **generic `async fn`** (async ports). One function per clause, named after the clause.
- A `#[macro_export] macro_rules! <port>_contract { ($make:expr) => { mod ... { #[test] fn clause() { c::clause(&$make) } ... } } }`
  generates one `#[test]` per clause, so CI shows *which* clause an adapter breaks.
- Each adapter crate: `tests/contract.rs` = `#![cfg(target_os = "...")]` + one macro line.
- The **fake** runs the same macro (in two configurations: fully capable and minimal), so core
  tests cannot rely on behaviour no real adapter has.
- Precedents: notify's generic test harness `channel<W: Watcher>()` used across watcher backends
  [primary: notify@69eac31 notify/src/test.rs:436]; keyring ships "unit-test" and "leak-test" example
  programs that every credential-store crate author is asked to run against their store, and a
  `keyring-core` `mock`/`sample` store [primary: keyring-rs README].

What to put in a contract (SystemProbe example, all implemented):
1. Capabilities stable over lifetime.
2. Behaviour matches capabilities (Unsupported => `None` / `Err(Unsupported)`; supported => works).
3. Value sanity (ranges, units).
4. **Cost budget** (`snapshot()` < 5 ms averaged over 50 calls) - energy is a contract.
5. Resource hygiene (dropping a subscription doesn't break the adapter).
6. For async ports: cancel safety (drop in-flight future, next call works), `TooLarge`/validation errors.
7. For event ports: bounded channel, never blocks producer, `dropped()` counts.
8. For permissioned ports (audio/screen): `permission_state()` consistent with `start()` result.

Real-adapter caveats: CI machines lack batteries, microphones, screen-recording permission (TCC).
Write clauses so absent hardware is legal (`None`), and keep permission-requiring clauses behind an
env var / `#[ignore]` run on a provisioned self-hosted Mac.

## 3. Fakes: rules

- In-memory, deterministic, **cloneable handle** (`Arc<Mutex<State>>`): test keeps one clone to script, core gets the other.
- Scriptable: set values, inject one-shot errors (`fail_next`), emit events, count calls.
- No sleeping, no threads, no real time. Async fakes complete immediately (`block_on_ready`
  executor with `Waker::noop()` - stable since Rust 1.85 [primary: compiled on 1.97]).
- Fakes pass the contract suite (both "healthy" and "minimal" presets).
- Fakes never live in production crates and testkit is never a normal dependency (archtest R2).

## 4. Virtual time for days-long behaviour

- Inject `Clock`; fake `VirtualClock::sleep` advances time and returns. The example runs a
  **3-day soak in microseconds** and asserts the poll count (~4320 at 60 s idle cadence), which is
  an energy property tested on every commit [primary: example test `three_day_soak_in_virtual_time_polls_slowly_when_stable`].
- For adapters built on tokio timers, use tokio's paused time (`#[tokio::test(start_paused = true)]`,
  `test-util` feature). Pitfall found here [primary]: a clock adapter that stores `std::time::Instant`
  does **not** advance under paused tokio time; use `tokio::time::Instant` inside `TokioClock`.
- Real soak on hardware remains necessary: measure CPU %, RSS, wakeups, energy impact (macOS
  Activity Monitor / `powermetrics` [secondary]) over >= 24-72 h before release; include sleep/wake cycles.

## 5. Things that are hard to fake - push them to the edges

- Run loops / main-thread-only APIs (AppKit, CFRunLoop): adapter owns the thread; port is plain data + channels.
- Realtime audio threads: adapter copies into pooled buffers and `try_send`s; core never sees the callback.
- OS permission prompts: model as `PermissionState` + `request_permission()`; test UI flow against fake states.
