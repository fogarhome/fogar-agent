//! fogar-agent-linux: ILLUSTRATIVE adapters proving the ports and contract suites work against a
//! real OS. Not the product target (macOS first); S8 turns this into the real Linux port.
#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

mod audio;
mod ipc;
mod power;
mod probe;
mod secrets;

pub use audio::{NoAudioSink, NoAudioSource};
pub use ipc::{UdsIpcClient, UdsIpcServer};
pub use power::LinuxPowerPolicy;
pub use probe::ProcSystemProbe;
pub use secrets::FileSecretStore;

/// Creates (or validates) a private directory: owned by us, mode 0700, not a symlink
/// (fogar-secure-rust IPC-1/IPC-5, SEC-1 file fallback).
pub fn ensure_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let md = std::fs::symlink_metadata(dir)?;
    let me = std::fs::metadata("/proc/self")?.uid();
    if md.file_type().is_symlink() || !md.is_dir() || md.uid() != me {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "not a private directory we own",
        ));
    }
    if md.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
