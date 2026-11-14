---
name: fogar-hexagonal-slices
description: Hexagonal architecture (ports & adapters) and vertical-slice delivery for the fogar-agent Rust workspace, a headless always-on cross-platform daemon (macOS first, then Windows/Linux). Use it when you create or change crates (fogar-proto, fogar-ports, fogar-core, fogar-testkit, fogar-host, fogar-agent-macos/-windows/-linux), design or change a port trait (audio source, screen capture, system probe, UI IPC, lifecycle/autostart, power policy/QoS, secrets, clock/timers, uplink), write an adapter, fake or contract test, decide between generics, dyn and enum dispatch or async fn in traits, add cfg(target_os) or Cargo features, wire the composition root, plan or finish a vertical slice (walking skeleton, definition of done), enforce dependency rules (cargo-deny, clippy disallowed-methods, archtest with cargo metadata/cargo tree), keep fogar-proto no_std+alloc, or split work across parallel AI agents in git worktrees.
---

# fogar: hexagonal workspace + vertical slices

Primary sources and confidence: `RESEARCH.md`. A compile-checked skeleton is in
`references/example-workspace/` (30 tests green, clippy clean, Rust 1.97 on Linux).

## 1. Layout (inner → outer)

| Crate | Contains | May depend on |
|---|---|---|
| `fogar-proto` | wire codec, `#![no_std]` + `alloc`, sans-I/O | thiserror (`default-features=false`) only |
| `fogar-ports` | port traits + value types crossing them, `Support`, `EventStream` | std, thiserror, `tokio` with only `"sync"` |
| `fogar-core` | engine: pure policy fns + orchestration over ports | ports, proto |
| `fogar-testkit` | fakes + contract suites (+ macro) | ports. **Dev-dependency only** |
| `fogar-host` | portable adapters (tokio clock, TCP/TLS uplink) | ports, tokio… no `cfg(target_os)` |
| `fogar-agent-<os>` | OS adapters + composition root `main.rs` | anything; OS crates under `[target.'cfg(..)'.dependencies]` |
| `fogar-archtest` | dependency-rule tests | serde_json (dev) |

`fogar-ports` is an addition to the original plan. It lets adapters depend on the contract
instead of the engine, gives contract-first PRs one small unit, and keeps core edits from
rebuilding adapters. It is fine to start with a `fogar_core::ports` module and split it out
when the second adapter crate appears.

## 2. Dependency rules (mechanical, never by convention)

- **R1** proto/ports/core never reach `fogar-agent-*`/`fogar-host` (check with `cargo tree -p X --target all -e normal,build`).
- **R2** testkit is only ever `kind = "dev"` (check with `cargo metadata`).
- **R3** only `fogar-agent-*` have target-specific deps.
- **R4** inner crates never reach OS crates (libc, objc2*, windows*, core-foundation*, zbus…).
- **R5** no `target_os`, `cfg(windows|unix)` or `cfg_select!` text in the inner crates.
- **R6** proto deps are allowlisted and set `default-features=false`.
- **R7** each inner crate has a `clippy.toml` with `disallowed-methods/types` (Instant::now, thread::sleep, env::var, fs, Command, TcpStream). Set `[workspace.lints.clippy] disallowed_methods = "deny"`.
- `deny.toml`: `[bans] deny = [{ crate = "objc2", wrappers = ["fogar-agent-macos", …] }]` and `[graph] targets` listing the darwin, windows and linux triples.
- Gotcha: `cargo metadata`'s resolve graph is **feature-unified across the whole workspace**, so it shows false transitive edges. Use `cargo tree -p` for transitive rules.
- The `cfg(target_os)` gate belongs in adapter crates and the composition root only. Gate a whole module or crate with `#![cfg(target_os = "macos")]` so the workspace builds on any host. Select adapters with a single `cfg_select!` (stable since 1.95).
- Never pick a platform with a Cargo feature. Features must be additive, and a feature must never swap in a fake.

## 3. Port design decision table

| Question | Default | Exception |
|---|---|---|
| Sync or async? | **sync** for cheap OS reads (probe, secrets, autostart, QoS) | async only for real I/O latency (uplink, IPC, capture start/stop) |
| Dispatch | `Box/Arc<dyn Port>` built once at the composition root | generic `P: Port` for per-buffer hot paths and all async ports; enum dispatch for closed sets that switch at runtime (cpal) |
| Async signature | `fn op(&self) -> impl Future<Output = R> + Send` (impls may write `async fn`) | Plain `async fn` in a pub trait triggers the `async_fn_in_trait` warning and gives no `Send` guarantee. Async methods are not dyn-compatible (E0038). RTN is still nightly. Need dyn? Add a blanket-implemented boxed twin (`DynUplink`) or `dynosaur` 0.3 |
| Trait bounds | `Send + Sync + 'static` | — |
| Dyn guard | `const _: Option<&dyn Port> = None;` in the port module | — |
| Granularity | ≤ ~5 methods, named for the core's need | Avoid a zed-style giant `Platform` trait |
| Data | port-owned value types; buffers/frames per call | never cross a port per sample or pixel (dyn sync ≈1.5 ns, boxed async ≈14 ns measured) |
| Events | bounded `EventStream<T>`. Adapter uses `try_send`, drops and counts when full, stops when closed | latest-only (`watch`) for frames; awaited send for IPC requests |
| Unsupported | `capabilities()` returns `Support::{Native, Emulated, Unsupported}`, plus `Option` fields, plus defaulted methods returning `Unsupported` | permission state is its own capability (TCC: mic, screen) |
| Errors | one `#[non_exhaustive]` thiserror enum per port: `Unsupported`, `PermissionDenied`, `Transient`, `Platform(#[source] Box<dyn Error+Send+Sync>)`, plus `is_retryable()` | never `#[cfg]` on variants |
| Cancellation | each async method documents cancel-safety. Core mutates state only after an await completes | contract-test "drop in-flight future, call again" |
| Platform strengths | model the richest OS (macOS thermal state, low-power mode) as optional capabilities | not lowest-common-denominator. Platform knobs go in the adapter config, not the port |
| Evolution | new defaulted method + new caps field defaulting to `Unsupported` + gated contract clause = 0 adapters break | a non-defaulted item, a change that breaks dyn-compat, or a new variant on an exhaustive enum is a breaking change (Cargo SemVer reference) |
| Clock | `Clock` port. State whether it counts system suspend (std `Instant` does not specify) | use tokio's `Instant` in the tokio clock adapter, or paused-time tests fail |

Details: `references/port-design.md`.

## 4. Vertical slice template

```
Slice: <name> | Outcome on a real Mac: <one sentence>
Rings: proto[ ] ports[ ] core[ ] testkit[ ] host[ ] agent-macos[ ] ipc/ui[ ]
1. Contract PR (merged FIRST): trait diff, caps flags (default Unsupported), fake, contract clauses, proto msgs+vectors
2. Core: pure decision fns + table tests; orchestration over ports; acceptance test with fakes + VirtualClock
3. Adapter(s): macOS impl + tests/contract.rs; other OSes report Unsupported explicitly
4. Composition root wiring + real-Mac smoke test; energy budget measured
```
Slice 0 is the walking skeleton: probe → governor → encode → uplink(stderr) → status over IPC,
plus archtest, clippy, deny and a CI matrix (Linux plus macOS). That is the example workspace.
More in `references/vertical-slices.md`.

## 5. Contract-test pattern

- In `fogar-testkit::contract::<port>` write one function per clause, taking `&dyn Port` (or `async fn<U: Port>`).
- Add a `#[macro_export] macro_rules! <port>_contract { ($make:expr) => { mod … { #[test] fn clause() {…} } } }`.
- Each adapter's `tests/contract.rs` is `#![cfg(target_os = "…")]` plus one macro line.
- The fake runs the same suite in both its "healthy" and "minimal" presets.
- Clauses to cover:
  - caps stay stable;
  - behaviour matches caps;
  - values are sane;
  - a **cost budget** holds (e.g. `snapshot` < 5 ms);
  - dropping a subscription is safe;
  - the adapter survives a cancelled call;
  - the producer never blocks.
- Run a days-long soak in virtual time (`VirtualClock`) and assert the poll count. That is an energy contract.

Details: `references/testing-fakes-contracts.md`.

## 6. Anti-patterns

- `cfg(target_os)` or per-OS type aliases in core or ports (zed's `PlatformScreenCaptureFrame` leaks this way).
- Per-OS knobs in port options (espanso's `win32_*` fields).
- `#[cfg(unix)]` error variants in shared crates (mullvad's management interface).
- Lowest-common-denominator ports that hide macOS strengths. The opposite failure is speculative generality: capability fields with no known platform difference.
- A single giant platform trait, or ports named after OS APIs.
- Fakes that skip the contract suite. Testkit as a normal dependency. `test`/`mock` Cargo features that swap implementations.
- Blocking OS callback threads on a full channel. Unbounded channels in a days-long daemon.
- Calling `Instant::now`, `sleep` or `env::var` directly in core.
- Agents editing archtest, clippy or deny rules, or another crate's code, to get green.
- Building layer by layer (all ports, then all adapters) instead of slices.

**YAGNI rule**: put behind a port anything that touches the OS or the network, plus every source
of nondeterminism (time, randomness, IDs). Do not port pure logic. Shape each port from one real
adapter (macOS) plus a fake. Before freezing a port, write a 10-line "how would Windows/Linux do
this?" note, and add a capability only for a known difference. Extract shared adapter code into
`fogar-host` only once a second adapter crate needs it.

## 7. Parallel AI agents

- Each crate has one owner per slice. Contract PRs (ports + testkit + proto) come first and get human review. After that, fan out worktrees for core, each adapter and the UI/IPC side.
- Agents edit only the crates they own. New third-party deps go through `[workspace.dependencies]` and the architecture owner.
- `Cargo.lock` conflicts: regenerate the lockfile, never merge it by hand. Give each worktree its own target dir.
- An agent on Linux cannot type-check macOS-gated code. macOS CI must be green before merge.

## 8. Checklist (every PR)

- [ ] No new `cfg(target_os)` outside adapter crates and the composition root. Adapter crates still build on other hosts.
- [ ] Port changes are additive: defaulted methods, `#[non_exhaustive]`, caps default to `Unsupported`. The dyn guard still compiles.
- [ ] Fake and contract suite are updated in the same PR as the trait. The fake passes the suite.
- [ ] Async methods: `+ Send`; cancel-safety is documented and tested.
- [ ] Event producers never block. Channels are bounded and drops are counted.
- [ ] Errors are mapped to port categories. No panics on OS errors.
- [ ] Energy: adaptive polling, no busy loops, cost budget asserted.
- [ ] `fogar-proto` still `no_std`; wire changes have vectors and truncation tests.
- [ ] `cargo fmt --check && cargo clippy --workspace --all-targets && cargo test -p <owned> -p fogar-archtest` pass, and `cargo deny check` passes.
- [ ] The slice DoD in `references/vertical-slices.md` is met before the slice is called done.

References: `references/workspace-and-dependency-rules.md`, `references/port-design.md`,
`references/vertical-slices.md`, `references/testing-fakes-contracts.md`, `references/no-std-proto.md`,
`references/real-world-patterns.md` (zed, espanso, cpal, keyring, notify, sysinfo, mullvad, rust-analyzer, with pinned links).
