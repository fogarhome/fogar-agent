# ADR-0004: No UniFFI; separate processes share a wire contract, not code

Status: **PROVISIONAL**

## Decision
The Swift UI and the Rust agent are separate processes talking over the IPC socket (ADR-0002).
They share the message contract, not a linked library: no UniFFI, no Rust staticlib in the app.
Swift `Codable` types will be generated with typeshare from the IPC wire types (S1), with
hand-written Swift framing (4-byte BE length + JSON). Revisit UniFFI only if the Swift client
grows past ~500 lines of protocol code (fogar-macos-daemon §1).

## Slice 0 state
Wire types exist only in `fogar-agent-linux` (`WireRequest`/`WireResponse`); S1 moves them to a
shared, typeshare-annotated module.

## To verify on the M2
- typeshare output compiles in the Swift app target and round-trips against the agent.
- Version handshake (`hello{version}`) rejects mismatched UI/agent pairs after a Sparkle update.

## Amendment (2026-11-14, S1 contract PR)
The wire types live in `fogar-host/src/ipc_wire.rs`, marked `#[cfg_attr(typeshare, typeshare)]`
(`cfg(typeshare)` is never set, so there is no runtime dependency; the typeshare CLI reads the
attribute). Shape forced by typeshare 1.13: enums with data are adjacently tagged
`{"type": ..., "data": {...}}` with one struct per variant, data-less variants are `{"type": ...}`,
`u64` counters go to Swift `Int`. Verified: `typeshare crates/fogar-host/src --lang=swift` output
type-checks and decodes the Rust golden JSON (and Rust decodes Swift's output). The Swift enums are
closed, so any new enum value or shape change is an `IPC_VERSION` bump; the `hello` handshake
answers with the agent's version so the app can tell which side to update.
