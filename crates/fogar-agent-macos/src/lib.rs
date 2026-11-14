//! fogar-agent-macos. S1: Keychain `SecretStore` and Unix-socket `IpcServer` with the same-signer
//! peer check are real and contract-tested on macOS (CI runner + the M2). Audio is still a stub
//! and the probe/power adapters partial; every capability not implemented reports `Unsupported`
//! (contract-checked by `tests/contract.rs`); TODOs name the exact Apple API from the skills.
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
pub use ipc::{COMPANION_APP_ID, MacIpcClient, MacIpcConn, MacIpcServer, ensure_private_dir};
pub use power::MacPowerPolicy;
pub use probe::MacSystemProbe;
pub use secrets::{KEYCHAIN_SERVICE, MacKeychainStore};
