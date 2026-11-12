//! SecretStore port: long-lived credentials (the gateway `client_key`; SSH material later).
//!
//! - Values cross the port as `secrecy::SecretString` (redacted `Debug`, zeroized on drop).
//! - Sync: Keychain / Credential Manager / Secret Service reads are short. Adapters MUST NOT
//!   prompt the user from these calls (prompts belong to explicit UI actions).
//! - macOS: Keychain generic password (service `com.fogar.agent`). Windows: Credential Manager.
//!   Linux: Secret Service (zbus) or a 0600 file under `$XDG_STATE_HOME` as `Emulated`.

use crate::Support;
pub use secrecy::{ExposeSecret, SecretString};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SecretKey {
    /// The gateway's per-device bearer key (spec §2). Shown once at pairing; never logged.
    GatewayClientKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SecretCaps {
    /// Survives a restart.
    pub persistent: Support,
}

impl SecretCaps {
    pub const NONE: Self = Self { persistent: Support::Unsupported };
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SecretError {
    #[error("not supported by this adapter: {0}")]
    Unsupported(&'static str),
    /// e.g. Keychain locked; worth retrying later.
    #[error("secret store temporarily unavailable")]
    Unavailable,
    #[error("secret store failure")]
    Platform(#[source] Box<dyn std::error::Error + Send + Sync>),
}

pub trait SecretStore: Send + Sync + 'static {
    fn capabilities(&self) -> SecretCaps;
    /// `Ok(None)` = not stored (e.g. not paired yet).
    fn get(&self, key: SecretKey) -> Result<Option<SecretString>, SecretError>;
    fn put(&self, key: SecretKey, value: SecretString) -> Result<(), SecretError>;
    /// Deleting a missing key is `Ok(())`.
    fn delete(&self, key: SecretKey) -> Result<(), SecretError>;
}

const _: Option<&dyn SecretStore> = None;
