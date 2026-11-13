//! fogar-agent-macos — **UNTESTED in slice 0.** Written on Linux; never compiled or run here.
//! The macOS CI job (`cargo check --workspace`, `cargo test --workspace`) is the first compiler
//! to see this code. Every capability not implemented reports `Unsupported` (contract-checked by
//! `tests/contract.rs` on the macOS runner); TODOs name the exact Apple API from the skills.
//!
//! All `unsafe` FFI of the agent lives in `fogar-agent-*` crates (fogar-secure-rust UNSAFE-1);
//! each block carries a `SAFETY:` comment.
#![cfg(target_os = "macos")]
#![deny(unsafe_op_in_unsafe_fn)]

mod audio;
mod ipc;
mod power;
mod probe;
mod secrets;

pub use audio::{MacAudioSink, MacAudioSource};
pub use ipc::MacIpcServer;
pub use power::MacPowerPolicy;
pub use probe::MacSystemProbe;
pub use secrets::MacKeychainStore;
