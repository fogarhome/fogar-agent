# Example workspace (compile-checked)

A minimal, working walking skeleton of the fogar-agent architecture. Verified on 2026-11-12 with
rustc/cargo 1.97.0 on Linux x86_64:
`cargo build --workspace --all-targets`, `cargo test --workspace` (30 tests: proto round-trip,
fake + procfs contract suites, core slice tests incl. 3-day virtual soak, host adapter contract,
6 archtest rules) and `cargo clippy --workspace --all-targets` (clean) all pass.
The macOS module (`crates/fogar-agent-macos/src/mac.rs`) and its contract test are cfg-gated and
were NOT compiled here.

```
Cargo.toml                    workspace: resolver 3, edition 2024, rust-version 1.95, shared deps + lints
deny.toml                     cargo-deny bans (objc2/windows only via adapter crates)  [not executed]
crates/
  fogar-proto/                no_std + alloc codec (PLACEHOLDER framing, not the gateway protocol)
  fogar-ports/                SystemProbe (sync, dyn), Uplink (async RPITIT + DynUplink twin), Clock, EventStream, Support
  fogar-core/                 Governor slice: pure decide() + tick() over ports; clippy.toml bans ambient std APIs
    tests/governor_slice.rs   slice acceptance test with fakes + 3-day virtual-time soak
  fogar-testkit/              fakes (FakeSystemProbe, RecordingUplink, VirtualClock) + contract suites + macro
    tests/fakes_pass_contracts.rs
  fogar-host/                 portable adapters: TokioClock, StderrUplink (+ uplink contract under tokio paused time)
  fogar-agent-macos/          macOS adapter (cfg-gated) + composition root main.rs (cfg_select!)
  fogar-agent-linux/          ILLUSTRATIVE procfs adapter: proves the contract suite runs against a real OS
  fogar-archtest/             R1-R6 dependency rules as cargo tests
```

Run (from this directory): `cargo test --workspace && cargo clippy --workspace --all-targets`.
Use a separate `CARGO_TARGET_DIR` per worktree.
