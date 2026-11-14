# Workspace layout, dependency rules, and mechanical enforcement

## 1. Crate roles (inner -> outer)

```
fogar-proto        no_std + alloc. Wire codec only. Depends on nothing of ours.
fogar-ports        Port traits + the value types that cross them. Depends on: (std), thiserror, tokio/"sync".
fogar-core         Engine/policy. Depends on: fogar-ports, fogar-proto. No OS, no cfg(target_os), no runtime.
fogar-testkit      Fakes + contract suites. Depends on: fogar-ports. DEV-DEPENDENCY ONLY.
fogar-host         Portable adapters (tokio clock, TCP/TLS uplink, file-based config). No cfg(target_os).
fogar-agent-macos  macOS adapters + composition root (bin). Only crate allowed objc2/IOKit/etc.
fogar-agent-windows / fogar-agent-linux   later; same shape.
fogar-archtest     Architecture fitness tests (cargo test). publish = false.
```

Why a separate `fogar-ports` (an addition to the planned proto/core/agent-* list):
- Adapters depend on the **contract**, not the engine: an edit in core does not rebuild or
  re-review every adapter, and adapter worktrees cannot reach into engine internals.
- It is the unit of "contract-first" PRs for parallel agents (see vertical-slices.md).
- Pragmatic alternative if you want fewer crates at first: `fogar_core::ports` module, split out
  the day a second adapter crate exists. The archtest rules below work either way.

Real-world precedent for "one crate per platform + thin selector crate": zed split gpui into
`gpui` (traits incl. `Platform`), `gpui_macos`, `gpui_windows`, `gpui_linux`, `gpui_web`, and
`gpui_platform` whose `current_platform()` is the only `cfg_select!` over OSes
[primary: zed@8e474e1 crates/gpui_platform/src/gpui_platform.rs:62]. keyring v4 split into
`keyring-core` (API + `mock`/`sample` stores) and one crate per credential store
(`apple-native-keyring-store`, `windows-native-keyring-store`, `dbus-secret-service-keyring-store`, ...)
[primary: keyring-rs@03f7683 Cargo.toml]. Espanso keeps a pure `espanso-engine` (dozens of
small port traits: `TextInjector`, `NotificationManager`, `funnel::Source`...) whose deps are
only log/anyhow/thiserror/crossbeam [primary: espanso@80d9e35 espanso-engine/Cargo.toml].

## 2. Dependency rules

| # | Rule | Enforced by |
|---|---|---|
| R1 | proto, ports, core never depend (normal/build, transitively, any target) on `fogar-agent-*` / `fogar-host` | archtest (`cargo tree -p X --target all -e normal,build`) |
| R2 | `fogar-testkit` only as `[dev-dependencies]` | archtest (`cargo metadata` `dependencies[].kind`) |
| R3 | only `fogar-agent-*` declare `[target.'cfg(..)'.dependencies]` | archtest (`dependencies[].target` non-null) |
| R4 | inner crates never reach OS-binding crates (libc, objc2*, windows*, core-foundation*, zbus, ...) | archtest + cargo-deny `wrappers` |
| R5 | no `target_os`/`cfg(windows)`/`cfg(unix)`/`cfg_select!` text in inner crate sources | archtest (text scan) |
| R6 | fogar-proto: allowlisted deps, all `default-features = false` | archtest |
| R7 | core calls no ambient-authority std APIs (`Instant::now`, `thread::sleep`, `env::var`, `fs::*`, `Command`) | per-crate `clippy.toml` `disallowed-methods/types` |
| R8 | core modules acyclic | `cargo modules dependencies --lib --acyclic` (cargo-modules 0.28; optional) |

All of R1-R6 are implemented and passing in `example-workspace/crates/fogar-archtest/tests/deps.rs`
(6 tests) [primary: ran here].

### Pitfall found while building the archtest [primary]
`cargo metadata`'s `resolve` graph is **feature-unified across the whole workspace**. Walking it
reported `fogar-ports -> tokio -> libc, windows-sys` only because `fogar-agent-macos` enables
`tokio/signal`. `cargo tree -p fogar-ports --target all -e normal` resolves features for that
package alone and shows `tokio -> pin-project-lite` only. Use `cargo tree -p` (or guppy) for
transitive rules; use `cargo metadata` `packages[].dependencies` for declared-edge rules.

### Pitfall: building subsets rebuilds shared deps [primary]
Clean `cargo build --workspace --all-targets` of the example: ~18 s. Then
`cargo build -p fogar-proto -p fogar-ports -p fogar-core` on the same target dir: ~3.7 s of
rebuilding, because tokio etc. resolve with different feature sets for the subset. Cargo's docs
note resolver-2 de-unification "can increase build times because the dependency is built multiple
times" [primary: cargo features reference]. Mitigation: `cargo-hakari` workspace-hack crate
("speed up local cargo build and cargo check commands by up to 100x, and cumulatively by up to 1.7x")
[primary: cargo-hakari 0.9.39 README] - adopt only when measured pain appears.

## 3. cargo-deny (0.20.x) for third-party bans [primary: cargo-deny@0fabb18 docs + src]

- `[bans] deny = [{ crate = "objc2", wrappers = ["fogar-agent-macos", ...] }]`: "allows specific
  crates to have a direct dependency on the banned crate but denies all transitive dependencies on it".
  Implementation checks every *direct dependent* of the banned crate; workspace members are subject
  to bans unless `allow-workspace = true` (src/bans.rs).
- `use-instead = "rustls"`, `reason = "..."`, `deny-multiple-versions = true` exist.
- `[graph] targets = [...]`: limits target-specific edges to the triples you ship; list
  `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu` so Linux CI still
  evaluates macOS/Windows-only deps.
- `wrappers` on very common sys crates (`libc`, `windows-sys`) is fragile (tokio, mio, getrandom
  depend on them directly). Ban the *high-level* OS binding crates (objc2 family, `windows`, IOKit
  wrappers) with wrappers; cover the rest with archtest R4.
- Not executed here (policy: no running downloaded tools). Generate the final wrappers list from a
  real `cargo deny check bans` run.

## 4. clippy `disallowed-*` per crate [primary: verified here]

`crates/fogar-core/clippy.toml` with `disallowed-methods = [{ path = "std::time::Instant::now", reason = "..." }, ...]`
plus `[workspace.lints.clippy] disallowed_methods = "deny"`: injecting `std::time::Instant::now()` into
fogar-core produced `error: use of a disallowed method std::time::Instant::now` with the reason note,
while the same call in `fogar-host` (no clippy.toml) passed. So clippy picks up the crate-local
`clippy.toml`; adapters stay unrestricted.

## 5. Where `cfg(target_os)` is allowed

- Allowed: adapter crates (`fogar-agent-*`), the composition root, `[target.'cfg(..)'.dependencies]`
  of adapter crates, and test files of adapter crates.
- Forbidden: proto, ports, core, testkit, host (archtest R5 greps).
- Make adapter crates **compile to nothing** on other hosts: `#![cfg(target_os = "macos")]` at
  module/crate level + OS deps under `[target.'cfg(target_os = "macos")'.dependencies]`
  (zed's `gpui_macos` does both [primary: gpui_macos.rs line 1, Cargo.toml:26]). Then
  `cargo check/clippy/test --workspace` works on every host and in every agent worktree.
- `cfg_select!` is stable since **Rust 1.95** (2026-04-16) [primary: Rust 1.95.0 announcement title;
  used by sysinfo 0.40.3 with `rust-version = "1.95"` and by zed on 1.98.1]. Replaces `cfg_if!`.
- Selecting adapters at the composition root: one `cfg_select!` block (zed `current_platform`), or
  a type alias per OS like notify's `RecommendedWatcher` [primary: notify lib.rs:619-648].
- Caveat for AI agents: code behind an inactive `cfg` is not type-checked or analysed on that host.
  A Linux worktree cannot validate macOS adapter code; macOS CI is the only truth.
  (`cargo check --target aarch64-apple-darwin` from Linux should work for pure-Rust deps like objc2
  but fails for build scripts needing the Apple SDK [unverified here: target std not installable in sandbox].)

## 6. Feature-flag pitfalls

Cargo: "features should be *additive*... enabling a feature should not disable functionality";
mutually exclusive features "should be avoided if at all possible" [primary: cargo features reference].
For fogar:
- **Never select the platform with a feature.** Use `cfg(target_os)` + target-specific deps. A
  `macos` feature can be enabled on Windows by unification.
- No `test`/`mock` feature on production crates to swap implementations (mullvad's `talpid-time` has
  `test = []` to swap `Instant` [primary: talpid-time/Cargo.toml:18]; it works only because dev-dep
  features stay out of normal builds under resolver 2, but it is fragile). Prefer a Clock **port** + fake.
- Optional capabilities (e.g. `screen-capture` in zed) are acceptable *in adapter crates* to cut
  build/link weight; then the port reports `Unsupported` when compiled out (zed: default method says
  "gpui was compiled without the screen-capture feature" [primary: platform.rs:438-446]).
- Test feature powerset of adapter crates in CI (`cargo hack --feature-powerset`) [secondary] once
  there are >2 features.
- `resolver = "3"` (edition 2024 default) adds MSRV-aware dependency resolution [secondary]; set
  `rust-version` in `[workspace.package]`.

## 7. Composition root / DI without frameworks

- One `main.rs` per agent binary: build adapters (with their platform config), build core with
  them, start runtime, wire shutdown signals. No service locator, no global registry, no DI crate.
- Generic engine types are named once here (`Governor<MacSystemProbe, TcpUplink, TokioClock>`);
  use `Box<dyn Port>` where you want runtime fallback (`MacProbe::new().map(boxed).unwrap_or_else(|_| Box::new(NullProbe))`).
- Runtime choice: a mostly idle daemon fits `tokio::runtime::Builder::new_current_thread()` (one
  thread, fewer wakeups). Adapters that need OS run loops (CFRunLoop for notifications, CoreAudio
  IO threads) own their threads and talk to core through `EventSink`.
- Espanso's binary crate holds the glue that implements engine traits on top of platform crates
  (`espanso/src/cli/worker/engine/...`) [primary] - the composition-root-plus-glue pattern.

## 8. Compile time and CI cost

- Generic ports monomorphise per adapter: few adapters per binary (1 real + fakes in tests) -> cost is low.
- Per-OS CI matrix: Linux runner for everything host-agnostic (fmt, clippy, all inner tests,
  archtest, cargo-deny, fake contract tests); macOS runner for `fogar-agent-macos` tests + contract
  tests; Windows later. macOS runner minutes are the most expensive on hosted CI [secondary]; keep
  macOS jobs to `-p fogar-agent-macos`.
- Cache: `Swatinem/rust-cache` or sccache [secondary]. Share one target dir per worktree, not across
  concurrently building worktrees (Cargo takes a build-directory lock). `build.build-dir` (separating
  intermediate artifacts from `target-dir`) is accepted by cargo 1.97 [primary: ran here; stabilization
  version not confirmed].
- `[profile.release] lto = "thin", codegen-units = 1, panic = "abort"` for a small, predictable daemon;
  supervisor (launchd/systemd/SCM) restarts on abort.
