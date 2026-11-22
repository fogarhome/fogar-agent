//! fogar-agent-macos. S1: Keychain `SecretStore` and Unix-socket `IpcServer` with the same-signer
//! peer check are real and contract-tested on macOS (CI runner + the M2). S2: real audio
//! (cpal/CoreAudio capture and playback, AVCaptureDevice permission), contract-tested on the macOS
//! CI runner, behaviour still to verify on the M2. The probe/power adapters are partial; every
//! capability not implemented reports `Unsupported` (contract-checked by `tests/contract.rs`);
//! TODOs name the exact Apple API from the skills.
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
mod tailnet;

pub use audio::{AudioCounters, MacAudioSink, MacAudioSource, audio_counters};
pub use ipc::{COMPANION_APP_ID, MacIpcClient, MacIpcConn, MacIpcServer, ensure_private_dir};
pub use power::MacPowerPolicy;
pub use probe::MacSystemProbe;
pub use secrets::{KEYCHAIN_SERVICE, MacKeychainStore};
pub use tailnet::MacTailnet;
