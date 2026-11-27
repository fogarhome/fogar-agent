//! Per-instance names derived from an explicit `--state-dir` (measurement runs start several
//! agents side by side, each with its own state directory).
//!
//! The macOS Keychain is per user, not per directory: without this every instance would share the
//! default item (`com.fogar.agent` / `gateway_account`) and one instance's sign-in or sign-out
//! would change the others'. With an explicit state directory other than the default one, the
//! agent uses its own Keychain service, derived from the directory so the same directory always
//! finds its own item again. The default (no `--state-dir`, or the default path spelled out)
//! keeps the frozen service name (fogar-macos-daemon hard rule 2). Paths are compared and hashed
//! canonical (`keychain_service_canonical`).

use std::path::Path;

/// [`keychain_service`] on the canonical paths (symlinks and `..` resolved), so two spellings of
/// one directory share one service and the default directory is recognised however it is
/// written. `state_dir` must exist (call after `ensure_private_dir`); a `default` that does not
/// exist yet cannot be the same directory and is ignored.
pub(crate) fn keychain_service_canonical(
    base: &str,
    state_dir: &Path,
    default: Option<&Path>,
) -> std::io::Result<String> {
    let dir = std::fs::canonicalize(state_dir)?;
    let default = default.and_then(|d| std::fs::canonicalize(d).ok());
    Ok(keychain_service(base, &dir, default.as_deref()))
}

/// The Keychain service for `state_dir`: `base` when it is the default state directory (or there
/// is none), else `<base>.dir-<16 hex digits>`, a stable FNV-1a 64 hash of the path's bytes.
///
/// FNV-1a and not `DefaultHasher`: its output is not guaranteed across Rust releases, and a
/// different service after a toolchain update would lose the account. The hash only separates
/// instances; it guards nothing (all instances run as the same user and could read each other's
/// items anyway).
pub(crate) fn keychain_service(base: &str, state_dir: &Path, default: Option<&Path>) -> String {
    if default == Some(state_dir) {
        return base.to_owned();
    }
    format!("{base}.dir-{:016x}", fnv1a64(path_bytes(state_dir)))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn path_bytes(p: &Path) -> &[u8] {
    std::os::unix::ffi::OsStrExt::as_bytes(p.as_os_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "com.fogar.agent";

    #[test]
    fn default_dir_keeps_the_frozen_service() {
        let d = Path::new("/Users/x/Library/Application Support/com.fogar.agent");
        assert_eq!(keychain_service(BASE, d, Some(d)), BASE);
    }

    #[test]
    fn explicit_dirs_get_their_own_stable_service() {
        let default = Path::new("/Users/x/Library/Application Support/com.fogar.agent");
        let a = keychain_service(BASE, Path::new("/tmp/fogar-a"), Some(default));
        let b = keychain_service(BASE, Path::new("/tmp/fogar-b"), Some(default));
        assert_ne!(a, BASE);
        assert_ne!(a, b);
        // Stable: the same directory finds the same item in a later run (pinned value, so a
        // change of hash is a visible, deliberate break).
        assert_eq!(
            a,
            keychain_service(BASE, Path::new("/tmp/fogar-a"), Some(default))
        );
        assert_eq!(a, "com.fogar.agent.dir-38520729324438e5");
        // No default directory (HOME unset): an explicit directory still gets its own service.
        assert_eq!(a, keychain_service(BASE, Path::new("/tmp/fogar-a"), None));
    }

    /// `..` and a symlink to the same directory give the same service; a symlink to the default
    /// directory keeps the frozen one.
    #[test]
    fn canonical_paths_share_one_service() {
        let base = std::env::temp_dir().join(format!("fogar-inst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let real = base.join("real");
        let default = base.join("default");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        std::fs::create_dir_all(&default).unwrap();
        std::os::unix::fs::symlink(&real, base.join("link")).unwrap();
        std::os::unix::fs::symlink(&default, base.join("link-default")).unwrap();
        let svc = |p: &Path| keychain_service_canonical(BASE, p, Some(&default)).unwrap();
        let direct = svc(&real);
        assert_ne!(direct, BASE);
        assert_eq!(svc(&real.join("sub/..")), direct);
        assert_eq!(svc(&base.join("link")), direct);
        assert_eq!(svc(&base.join("link-default")), BASE);
        assert_eq!(svc(&default.join("../default")), BASE);
        // A default that does not exist is never "the same directory".
        let missing = base.join("missing");
        assert_eq!(
            keychain_service_canonical(BASE, &real, Some(&missing)).unwrap(),
            direct
        );
        // The state directory itself must exist.
        assert!(keychain_service_canonical(BASE, &missing, Some(&default)).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn fnv1a64_known_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }
}
