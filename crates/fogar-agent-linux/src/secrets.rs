//! File-backed secret store for headless Linux (`Emulated`): one file per key, mode 0600, in a
//! private 0700 directory. The real Linux target (S8) prefers Secret Service; macOS uses Keychain.

use fogar_ports::{
    Support,
    secrets::{ExposeSecret, SecretCaps, SecretError, SecretKey, SecretStore, SecretString},
};
use std::{
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct FileSecretStore {
    dir: PathBuf,
}

fn platform(e: std::io::Error) -> SecretError {
    SecretError::Platform(Box::new(e))
}

impl FileSecretStore {
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        crate::ensure_private_dir(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    fn path(&self, key: SecretKey) -> PathBuf {
        let name = match key {
            SecretKey::GatewayClientKey => "gateway_client_key",
            SecretKey::GatewayPairing => "gateway_pairing",
            _ => "unknown_key",
        };
        self.dir.join(name)
    }
}

impl SecretStore for FileSecretStore {
    fn capabilities(&self) -> SecretCaps {
        let mut c = SecretCaps::NONE;
        c.persistent = Support::Emulated;
        c
    }

    fn get(&self, key: SecretKey) -> Result<Option<SecretString>, SecretError> {
        let p = self.path(key);
        let md = match std::fs::symlink_metadata(&p) {
            Ok(md) => md,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(platform(e)),
        };
        let me = std::fs::metadata("/proc/self").map_err(platform)?.uid();
        if !md.is_file() || md.uid() != me || md.permissions().mode() & 0o077 != 0 {
            return Err(SecretError::Platform(
                "secret file must be a regular 0600 file owned by the agent user".into(),
            ));
        }
        // Zeroized on drop (review sec#7, SEC-2): only the SecretString copy outlives this call.
        let s = zeroize::Zeroizing::new(std::fs::read_to_string(&p).map_err(platform)?);
        let v = s.trim();
        Ok((!v.is_empty()).then(|| SecretString::from(v.to_owned())))
    }

    fn put(&self, key: SecretKey, value: SecretString) -> Result<(), SecretError> {
        let p = self.path(key);
        let tmp = p.with_extension("tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(platform)?;
        f.write_all(value.expose_secret().as_bytes())
            .map_err(platform)?;
        f.sync_all().map_err(platform)?;
        std::fs::rename(&tmp, &p).map_err(platform)
    }

    fn delete(&self, key: SecretKey) -> Result<(), SecretError> {
        match std::fs::remove_file(self.path(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(platform(e)),
        }
    }
}
