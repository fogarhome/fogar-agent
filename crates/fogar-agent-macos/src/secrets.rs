//! UNTESTED STUB: Keychain access reports `Unsupported` in slice 0, so a macOS build stays
//! `NotPaired` (health-reported, no reconnect loop).
//!
//! TODO(S1, fogar-secure-rust SEC-1..SEC-4): `security-framework` 3.7.0 generic password,
//! service `com.fogar.agent`, account `gateway_client_key` (`set_generic_password`,
//! `get_generic_password`, `delete_generic_password`); login keychain of the user session (the
//! agent is a per-user LaunchAgent); never trigger UI from these calls; map
//! `errSecInteractionNotAllowed` (locked) to `SecretError::Unavailable`.

use fogar_ports::secrets::{SecretCaps, SecretError, SecretKey, SecretStore, SecretString};

#[derive(Debug, Clone, Copy, Default)]
pub struct MacKeychainStore;

impl SecretStore for MacKeychainStore {
    fn capabilities(&self) -> SecretCaps {
        SecretCaps::NONE
    }
    fn get(&self, _key: SecretKey) -> Result<Option<SecretString>, SecretError> {
        Err(SecretError::Unsupported("keychain arrives in S1"))
    }
    fn put(&self, _key: SecretKey, _value: SecretString) -> Result<(), SecretError> {
        Err(SecretError::Unsupported("keychain arrives in S1"))
    }
    fn delete(&self, _key: SecretKey) -> Result<(), SecretError> {
        Err(SecretError::Unsupported("keychain arrives in S1"))
    }
}
