# RESEARCH: hexagonal architecture + vertical slices for the fogar-agent Rust workspace

Date: 2026-11-12. Method: shallow clones and crate tarballs, each in its own empty directory,
read only and never executed. Small experiments were compiled with the local toolchain
(rustc/cargo 1.97.0, x86_64 Linux). Labels: [primary] means official docs, compiler or cargo
behaviour observed here, or project source; [secondary] means a reputable summary;
[unverified] means reasoned but not checked.

## 1. Sources

### Rust language and tooling
| Claim | Source | Label |
|---|---|---|
| Latest announced stable is 1.99.1 (1.99.0 on 2026-10-01, 1.99.1 on 2026-10-15). No 1.100 post found. | blog.rust-lang.org/releases index | primary |
| 1.99 stabilised C variadics and VaList, among other things. Nothing on async, traits or dyn. | blog.rust-lang.org/2026/10/01/Rust-1.99.0 | primary |
| `cfg_select!` was stabilised in 1.95.0 (2026-04-16). | Rust 1.95.0 announcement (search result title); used by sysinfo (MSRV 1.95) and by zed (toolchain 1.98.1) | primary/secondary |
| `async fn` and RPITIT in traits have been stable since 1.75. | Rust blog 2023-12-21 (the redirect could not be fetched); the compiler accepts them | secondary + primary (behaviour) |
| `async_fn_in_trait` warns on pub traits, and rustc suggests desugaring to `-> impl Future + Send`. | rustc 1.97 output | primary |
| E0277: Send cannot be proven for an `async fn` trait future in generic code. E0038: an async method is not dyn compatible. | rustc 1.97 output | primary |
| RTN is still nightly. Stabilisation is blocked on the next-gen trait solver and planned for "late this year" together with TAIT. PR #138424 was closed. | goals.rust-lang.org/2026/rtn.html | primary |
| `Waker::noop` is usable on stable. | compiled here (stable since 1.85 per memory) | primary (behaviour) / secondary (version) |
| `std::time::Instant` uses CLOCK_UPTIME_RAW on Darwin, CLOCK_MONOTONIC on UNIX and QPC on Windows. Whether suspend counts is "not specified". | doc.rust-lang.org/std/time/struct.Instant.html | primary |
| SemVer rules: a non-defaulted trait item is major, a defaulted one possibly-breaking, a dyn-incompatible item major, an enum variant without non_exhaustive major, removing a feature major, MSRV possibly-breaking. | doc.rust-lang.org/stable/cargo/reference/semver.html | primary |
| Features should be additive. Avoid mutually exclusive features. Resolver 2 de-unifies target, build and dev features. | doc.rust-lang.org/stable/cargo/reference/features.html | primary |
| API guidelines C-OBJECT, C-SEALED, C-STRUCT-PRIVATE, C-NEWTYPE-HIDE, C-GOOD-ERR, C-SEND-SYNC, C-CALLER-CONTROL | rust-lang.github.io/api-guidelines/checklist.html | primary |
| `build.build-dir` is accepted by cargo 1.97 (tested). The constant reads "build.build-dir is now always enabled". | local cargo; nightly-rustc docs | primary (version of stabilisation unknown) |

### Crates (latest versions on the sparse index, 2026-11-12) [primary]
tokio 1.54.1, thiserror 2.0.21 (`#![no_std]` with a `std` feature), trait-variant 0.1.3,
dynosaur 0.3.1, async-trait 0.1.92, futures 0.3.34, serde_json 1.0.154, cargo_metadata 0.23.1,
postcard 1.1.3, minicbor 2.3.1, cargo-deny 0.20.2, cargo-modules 0.28.0 (MSRV 1.98),
cargo-depgraph 1.6.0, cargo-hakari 0.9.39, guppy 0.19.1, objc2 0.6.5, keyring-core 1.0.1
(1.0.0 was the version read), libc 0.2.191. bincode 3.0.0 is a tombstone: its lib.rs is a single `compile_error!`.

### Tool documentation and source [primary]
- **cargo-deny @0fabb18.**
  - Docs: `docs/src/checks/bans/cfg.md`, covering `deny`, `wrappers`, `allow-workspace`, `use-instead` and `deny-multiple-versions`.
  - Docs: `docs/src/checks/cfg.md`, covering `[graph] targets` and `exclude-dev`.
  - Source: `src/bans.rs` checks every direct dependent of a banned crate. Workspace members are banned unless `allow-workspace` is set.
  - Tests: `tests/test_data/allow_wrappers` uses path crates.
- **cargo-modules 0.28 README.** Its `dependencies --acyclic` mode fails on module cycles inside a crate. `orphans --deny` is also available.
- **cargo-depgraph 1.6 README.** It renders a graphviz graph from `cargo metadata`. It does visualisation only, not enforcement.
- **cargo-hakari 0.9.39 README.** It manages a workspace-hack crate, claiming "up to 100x" faster local builds and "1.7x" cumulatively.
- **tokio 1.54.1.**
  - `src/sync/mod.rs:440`: "All synchronization primitives provided in this module are runtime agnostic", with `_timeout` methods as the exception.
  - `mpsc::Receiver::recv` is documented as cancel safe.

### Real projects (commit pinned) [primary]
zed@8e474e1, espanso@80d9e35, cpal@40ff3ad (0.19.0), keyring-rs@03f7683 plus the keyring-core
1.0.0 crate, notify@69eac31, sysinfo@7757e92 (0.40.3), mullvadvpn-app@7e1c49d,
rust-analyzer@8cbebb0, scap@c03f15a, xcap@6b4ad60. Paths, line numbers and links are in
`references/real-world-patterns.md`.

### Experiments run here [primary]
1. AFIT lint, Send failure and dyn failure: files in /tmp/claude-0/-home-claude/98937c5a-7c5f-5821-8d01-72ba576d84e5/scratchpad/exp/afit/ (a.rs..e.rs, bench.rs; outside the skill).
2. Microbenchmark at opt-level 3 over 20M calls:
   - static async ≈ 0.6 ns/call;
   - boxed dyn async ≈ 13–15 ns/call;
   - dyn sync ≈ 1.5 ns/call.
3. Example workspace:
   - **Build, test, lint.** All targets build. `cargo test --workspace` passes 30 tests. clippy is clean.
   - **Per-crate `clippy.toml`.** Verified to deny `Instant::now` and `thread::sleep` in fogar-core only.
   - **Archtest pitfall.** The archtest found that the `cargo metadata` resolve graph is feature-unified across the workspace. It falsely reported fogar-ports → tokio → libc/windows-sys, so it was switched to `cargo tree -p`.
   - **Clock pitfall.** A tokio paused-time test failed when `TokioClock` used `std::time::Instant`. It was fixed by using `tokio::time::Instant`.
   - **Build cost.** A clean workspace build took about 18 s. Afterwards, building only the inner crates rebuilt shared deps (about 3.7 s) because features unify differently.

## 2. Confidence

| Area | Confidence | Why |
|---|---|---|
| Async-in-traits behaviour on stable | High | Compiler output observed directly |
| RTN/dyn-async timeline | Medium | The goal page states intent, not a commitment. The "Box notation for dyn async trait" goal page was unreadable here. |
| cargo-deny `wrappers` semantics | Medium-high | Docs and source were read but the tool was not run (policy). The wrappers lists in `deny.toml` must be regenerated from a real run. |
| clippy per-crate config | High | Verified |
| Archtest approach | High | Implemented and run. Its rules are only as good as its crate-name lists. |
| Real-project patterns | High | Read at pinned commits |
| macOS adapter code in the example | Low-medium | cfg-gated and not compiled here. `libc::getloadavg` exists for Apple targets [secondary]. |
| Energy claims (fewer wakeups with current_thread, adaptive polling) | Medium | Standard reasoning. Not measured on a Mac. |
| QoS mapping (macOS QoS / Windows EcoQoS / Linux nice) | Low | Not researched in depth. Marked unverified. |

## 3. Conflicts and tensions found

1. **YAGNI versus "hexagonal everywhere".** The user explicitly wants ports for all platform unknowns. The rule adopted is: port everything that touches the OS or is nondeterministic, but shape each port from one real adapter plus a fake, and add capabilities only for known differences.
2. **One trait versus many small traits.**
   - Zed (a single large `Platform`) and espanso/keyring (small traits) both work in production.
   - For parallel AI agents, small traits clearly win because each change touches fewer adapters.
3. **`async fn` versus a desugared signature.**
   - The compiler lint discourages `async fn` in pub traits.
   - Inside a single workspace you could `#[allow(async_fn_in_trait)]` instead.
   - We chose the desugared `+ Send` form because the daemon uses `Send` futures, and the form costs nothing.
4. **`cargo metadata` versus `cargo tree` for rules.** Metadata's resolve graph is feature-unified, which is fine for declared-edge rules but wrong for transitive rules. Neither cargo-depgraph nor tools built on unified metadata should be trusted for "does crate X pull Y".
5. **Test seams through features versus ports.**
   - Mullvad uses a `test` feature to swap `Instant`, which works under resolver 2.
   - The Cargo guidance (additive features) and our port approach argue against it.
6. **Where ports live.** The user planned `fogar-core` to hold the engine, and hexagonal purists put ports in the core. We recommend a separate `fogar-ports` crate for build isolation and agent ownership. Either works with the same archtest.
7. **The keyring repo URL.** The clone came from `hwchen/keyring-rs`, while the README links `open-source-cooperative/keyring-rs`. The commit hash is the anchor.

## 4. Open questions and biggest unknowns

1. **The gateway protocol spec.** Its framing, schema and versioning are unknown. `fogar-proto` in the example is a placeholder. Codec choice (postcard, minicbor, prost no_std, hand-written) depends on the spec, and golden vectors are needed.
2. **macOS specifics for each port**, none verified here:
   - which APIs (objc2-foundation for `NSProcessInfo.thermalState`, IOKit power sources, HIDIdleTime, ScreenCaptureKit, CoreAudio/AVAudioEngine);
   - their threading or run-loop requirements;
   - TCC permission flows for a headless LaunchAgent versus a LaunchDaemon. Microphone and screen-recording permission for a background agent with no UI is a serious open issue: the UI app may need to own the permission or launch the agent as a helper.
3. **Whether `cargo check --target aarch64-apple-darwin` works from Linux** for the chosen objc2 crates. This decides whether Linux-hosted agents can type-check macOS code. It could not be tested because target std is not installable in this sandbox.
4. **Dyn async traits on stable.** No date is known. Re-check before adding the first `dyn` async port. dynosaur is the bridge.
5. **CI cost of macOS runners** and whether a self-hosted Mac is needed for permission-gated contract tests (mic, screen) and for 72-hour energy soaks.
6. **IPC transport and schema for the UI.** Candidates are gRPC over a local socket or pipe (mullvad uses tonic + tipsy) or a lighter framed protocol shared with fogar-proto. This needs a decision on how far the UI is cross-platform.
7. **Tiny-device future.** fogar-proto is ready. If fogar-core must run on bare metal later, `std::time::Duration` is fine but `tokio::sync` in ports is not. That would need a channel abstraction or `embassy-sync`-style types [unverified]. Do not pay this cost now.
8. **The exact Cargo version that stabilised `build.build-dir`.**
