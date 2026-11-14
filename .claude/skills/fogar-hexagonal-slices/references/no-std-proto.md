# fogar-proto: no_std + alloc design

Goal: one codec usable by the macOS daemon today and by tiny devices later, testable anywhere.

## Rules
- `#![no_std]` + `extern crate alloc;` always; optional `std` feature only for conveniences
  (`#[cfg(feature = "std")] extern crate std;`). Default features **off** in the workspace dep
  (`fogar-proto = { path = ..., default-features = false }`); crates that want `std` enable it.
- **Sans-I/O**: functions over `&[u8]` / `&mut Vec<u8>` (or a `Write`-like trait you define). No
  sockets, no async, no timers, no runtime types. Framing/stream reassembly as a pure state machine
  (`Decoder::feed(&mut self, bytes) -> impl Iterator<Item = Result<Message>>`).
- Decode borrows (`Message<'a>` with `&'a [u8]` payloads) to avoid allocation on hot paths;
  provide `into_owned()` when needed.
- Encode into caller-provided buffers (`encode_into(&msg, &mut Vec<u8>)`) so a long-running
  daemon reuses allocations (C-CALLER-CONTROL).
- Errors: `#[non_exhaustive]` enum, `thiserror` with `default-features = false`
  (`thiserror 2.0.21` is `#![no_std]` with a `std` feature [primary: crate source]); errors
  implement `core::error::Error` (stable since 1.81 [secondary]).
- `#[non_exhaustive]` on message enums; explicit protocol version byte; unknown kinds -> error,
  never panic. Truncated input must be tested for every prefix length (example test does this).
- Integer endianness explicit (`to_le_bytes`), no `repr(C)` transmutes, `#![forbid(unsafe_code)]`.
- No `HashMap` (needs std's RandomState): use `alloc::collections::BTreeMap` or sorted `Vec`.
- No floats in the wire format unless the gateway mandates them.

## Implementing an EXISTING gateway protocol
- Treat the gateway spec as the contract: golden test vectors (bytes captured from the gateway or its
  reference implementation) under `fogar-proto/tests/vectors/`, round-trip + byte-exact encode tests.
- If the gateway uses a schema format, choose a no_std-capable codec crate only after checking it
  matches the spec byte-for-byte. Candidates seen on crates.io 2026-11-12: `postcard 1.1.3`
  (serde, no_std), `minicbor 2.3.1` (CBOR, no_std) [primary: index versions; no_std claims secondary].
  `bincode 3.0.0` is a tombstone: README "Bincode is now unmaintained", `lib.rs` is a single
  `compile_error!` [primary: crate source] - do not adopt.
- protobuf: `prost` supports `no_std` with `alloc` [secondary/unverified for current version].
- Fuzz the decoder (`cargo fuzz`) once real messages exist [secondary].

## Verifying no_std mechanically
- archtest R6: all proto deps `default-features = false`, closure allowlisted.
- CI: `cargo build -p fogar-proto --no-default-features --target thumbv7em-none-eabihf` (or any
  bare-metal target) once a tiny-device target is real [unverified here: target std not installable
  in sandbox]. Until then, `#![no_std]` itself guarantees the crate's own code doesn't name `std`.
- Keep `fogar-proto` out of `fogar-ports`' public types where possible: ports talk in domain
  types; the core translates to wire messages (keeps wire evolution independent of adapters).
