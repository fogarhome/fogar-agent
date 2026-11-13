//! The daemon's private state directory (review sec#1; fogar-secure-rust IPC-1/IPC-5, SEC-1 file
//! fallback, LOG-3). Everything the daemon persists (boot ledger, crash records, secrets file on
//! Linux, the IPC socket dir) lives under it, so it is validated BEFORE anything is read from or
//! written to it: owned by us, a real directory (not a symlink), mode 0700 (tightened if looser).
//! Files inside are opened with `O_NOFOLLOW` and created 0600; temp files with `create_new`.
//!
//! There is deliberately no predictable fallback (e.g. `/tmp/fogar-agent`) when `HOME` is unset:
//! another local user could pre-create it. The caller then refuses to start and asks for
//! `--state-dir`.

use std::{
    fs::{DirBuilder, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

fn denied(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, msg)
}

/// Creates `dir` (0700, parents too) or validates the existing one.
pub(crate) fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    if !dir.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "state directory must be an absolute path",
        ));
    }
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let md = std::fs::symlink_metadata(dir)?;
    if md.file_type().is_symlink() {
        return Err(denied("is a symlink"));
    }
    if !md.is_dir() {
        return Err(denied("is not a directory"));
    }
    if md.uid() != rustix::process::getuid().as_raw() {
        return Err(denied("is owned by another user"));
    }
    if md.permissions().mode() & 0o7077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// `OpenOptions` for a private file: never follows a symlink in the last path component, and
/// creates with mode 0600.
pub(crate) fn private_file() -> OpenOptions {
    let mut o = OpenOptions::new();
    o.mode(0o600)
        .custom_flags(i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits()).unwrap_or(0));
    o
}

/// Reads a private file without following a symlink. `NotFound` and a symlink both read as
/// "absent" (the latter with a warning on stderr).
pub(crate) fn read_private(path: &Path) -> Option<String> {
    match private_file().read(true).open(path) {
        Ok(mut f) => {
            let mut s = String::new();
            io::Read::read_to_string(&mut f, &mut s).ok().map(|_| s)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            eprintln!(
                "fogar-agentd: ignoring {} ({e}; symlinks are never followed)",
                path.display()
            );
            None
        }
    }
}

/// Writes `bytes` to a fresh temp file next to `path` (a stale temp or a planted symlink there is
/// removed first, never followed), fsyncs it and renames it over `path` (rename replaces a
/// symlink at `path` itself, never its target).
pub(crate) fn replace_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut f = private_file().write(true).create_new(true).open(&tmp)?;
    io::Write::write_all(&mut f, bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Review sec#7: no core dumps of the daemon (they would contain the client key and, from S2,
/// captured audio). Linux: RLIMIT_CORE = 0 (soft and hard). macOS: the same call is POSIX and
/// is made too; TODO(macos, S2): verify on the M2 that launchd-started processes honour it and
/// that the release build is not `get-task-allow` (fogar-secure-rust MAC-1).
pub(crate) fn disable_core_dumps() -> io::Result<()> {
    rustix::process::setrlimit(
        rustix::process::Resource::Core,
        rustix::process::Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )
    .map_err(io::Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_relative_and_symlinked_dirs_and_tightens_mode() {
        assert!(ensure_private_dir(Path::new("relative/dir")).is_err());
        let base = std::env::temp_dir().join(format!("fogar-sdu-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let real = base.join("real");
        ensure_private_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&real).unwrap();
        assert_eq!(
            std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::os::unix::fs::symlink(&real, base.join("link")).unwrap();
        assert!(ensure_private_dir(&base.join("link")).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
