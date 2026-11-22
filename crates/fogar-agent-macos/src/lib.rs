//! fogar-agent-macos. S1: Keychain `SecretStore` and Unix-socket `IpcServer` with the same-signer
//! peer check are real and contract-tested on macOS (CI runner + the M2). S2: real audio
//! (cpal/CoreAudio capture and playback, AVCaptureDevice permission); on the macOS CI runner the
//! audio contract passes through `NoDevice`/`PermissionDenied` (capture itself is not run there),
//! behaviour still to verify on the M2. The probe/power adapters are partial; every
//! capability not implemented reports `Unsupported` (contract-checked by `tests/contract.rs`);
//! TODOs name the exact Apple API from the skills.
//!
//! All `unsafe` FFI of the agent lives in `fogar-agent-*` crates (fogar-secure-rust UNSAFE-1);
//! each block carries a `SAFETY:` comment.
//!
//! On other hosts the crate is empty, except under `cfg(test)`, where the OS-free audio parts
//! (`rt`) are compiled so their unit tests run on Linux CI too.
#![cfg(any(target_os = "macos", test))]
#![deny(unsafe_op_in_unsafe_fn)]

mod rt;

#[cfg(target_os = "macos")]
mod audio;
#[cfg(target_os = "macos")]
mod ipc;
#[cfg(target_os = "macos")]
mod power;
#[cfg(target_os = "macos")]
mod probe;
#[cfg(target_os = "macos")]
mod secrets;
#[cfg(target_os = "macos")]
mod tailnet;

#[cfg(target_os = "macos")]
pub use audio::{AudioCounters, MacAudioSink, MacAudioSource, audio_counters};
#[cfg(target_os = "macos")]
pub use ipc::{COMPANION_APP_ID, MacIpcClient, MacIpcConn, MacIpcServer, ensure_private_dir};
#[cfg(target_os = "macos")]
pub use power::MacPowerPolicy;
#[cfg(target_os = "macos")]
pub use probe::MacSystemProbe;
#[cfg(target_os = "macos")]
pub use secrets::{KEYCHAIN_SERVICE, MacKeychainStore};
#[cfg(target_os = "macos")]
pub use tailnet::MacTailnet;
