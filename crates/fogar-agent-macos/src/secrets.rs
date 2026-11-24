//! Keychain secret store (fogar-secure-rust SEC-1..SEC-4): one generic-password item per
//! [`SecretKey`] in the user's login keychain, service `com.fogar.agent`.
//!
//! - **Never prompts.** Keychain UI is disabled for the whole process on first use
//!   (`SecKeychainSetUserInteractionAllowed(false)`, process-global by Apple's design, so it is
//!   never re-enabled: a per-call guard dropped on another thread could re-enable it mid-call).
//!   Calls that would need a dialog fail instead.
//! - Legacy (file) keychain, not the data-protection keychain: the latter needs a
//!   `keychain-access-groups` entitlement + provisioning profile, which an ad-hoc or Personal Team
//!   command-line agent does not have (`errSecMissingEntitlement`).
//! - Item ACLs (measured on the M2, macOS 26.5.1, 2026-11-14): a rebuild signed by the SAME
//!   signer (same designated requirement) reads and updates the items but cannot delete them
//!   (`errSecInvalidOwnerEdit`, -25244); `delete` then overwrites the item with a tombstone that
//!   `get` reports as absent. An ad-hoc rebuild (new cdhash) cannot read them at all
//!   (`errSecAuthFailed`, -25293 → `Platform`); recover with
//!   `security delete-generic-password -s com.fogar.agent -a <account>`.

use fogar_ports::{
    Support,
    secrets::{ExposeSecret, SecretCaps, SecretError, SecretKey, SecretStore, SecretString},
};
use security_framework::{
    base::Error as SecError,
    os::macos::keychain::SecKeychain,
    passwords::{delete_generic_password, get_generic_password, set_generic_password},
};
use std::sync::OnceLock;
use zeroize::Zeroizing;

/// Frozen identity (fogar-macos-daemon hard rule 2).
pub const KEYCHAIN_SERVICE: &str = "com.fogar.agent";

const ERR_ITEM_NOT_FOUND: i32 = -25300;
const ERR_INTERACTION_NOT_ALLOWED: i32 = -25308;
const ERR_NO_SUCH_KEYCHAIN: i32 = -25294;
const ERR_NO_DEFAULT_KEYCHAIN: i32 = -25307;
const ERR_INVALID_OWNER_EDIT: i32 = -25244;
/// Stored by `delete` when the ACL forbids removing the item; never a real value (values are
/// JSON records or base64 keys).
const TOMBSTONE: &[u8] = b" ";

#[derive(Debug, Clone)]
pub struct MacKeychainStore {
    service: String,
}

impl Default for MacKeychainStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MacKeychainStore {
    /// The agent's store (service [`KEYCHAIN_SERVICE`]).
    pub fn new() -> Self {
        Self::with_service(KEYCHAIN_SERVICE)
    }

    /// A store under another service name (contract tests: never touch the real items).
    pub fn with_service(service: &str) -> Self {
        Self {
            service: service.to_owned(),
        }
    }

    pub fn service(&self) -> &str {
        &self.service
    }
}

/// One account per key. A new `SecretKey` variant is `Unsupported` until it gets its own account
/// here (never a shared fallback account that two secrets would overwrite).
fn account(key: SecretKey) -> Result<&'static str, SecretError> {
    match key {
        SecretKey::GatewayClientKey => Ok("gateway_client_key"),
        SecretKey::GatewayPairing => Ok("gateway_pairing"),
        SecretKey::GatewayAccount => Ok("gateway_account"),
        _ => Err(SecretError::Unsupported(
            "secret key without a keychain account",
        )),
    }
}

/// Disables keychain UI for this process once; `Err` if that failed (then no call is made, since
/// it could prompt).
fn no_ui() -> Result<(), SecretError> {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    let ok = *DISABLED.get_or_init(|| match SecKeychain::disable_user_interaction() {
        Ok(guard) => {
            // Dropping the guard re-enables UI process-wide; the agent never wants it back.
            std::mem::forget(guard);
            true
        }
        Err(_) => false,
    });
    if ok {
        Ok(())
    } else {
        Err(SecretError::Platform(
            "could not disable keychain user interaction".into(),
        ))
    }
}

fn map_err(e: SecError) -> SecretError {
    if matches!(
        e.code(),
        ERR_INTERACTION_NOT_ALLOWED | ERR_NO_SUCH_KEYCHAIN | ERR_NO_DEFAULT_KEYCHAIN
    ) {
        // Locked keychain, or no login keychain yet (agent started before the user session):
        // worth retrying later.
        SecretError::Unavailable
    } else {
        SecretError::Platform(Box::new(e))
    }
}

impl SecretStore for MacKeychainStore {
    fn capabilities(&self) -> SecretCaps {
        let mut c = SecretCaps::NONE;
        c.persistent = Support::Native;
        c
    }

    fn get(&self, key: SecretKey) -> Result<Option<SecretString>, SecretError> {
        no_ui()?;
        let bytes = match get_generic_password(&self.service, account(key)?) {
            Ok(b) => Zeroizing::new(b),
            Err(e) if e.code() == ERR_ITEM_NOT_FOUND => return Ok(None),
            Err(e) => return Err(map_err(e)),
        };
        if bytes.is_empty() || bytes[..] == *TOMBSTONE {
            return Ok(None);
        }
        let s = std::str::from_utf8(&bytes)
            .map_err(|_| SecretError::Platform("keychain item is not UTF-8".into()))?;
        Ok(Some(SecretString::from(s.to_owned())))
    }

    fn put(&self, key: SecretKey, value: SecretString) -> Result<(), SecretError> {
        no_ui()?;
        let v = value.expose_secret().as_bytes();
        if v.is_empty() || v == TOMBSTONE {
            return Err(SecretError::Platform(
                "refusing to store an empty secret".into(),
            ));
        }
        set_generic_password(&self.service, account(key)?, v).map_err(map_err)
    }

    fn delete(&self, key: SecretKey) -> Result<(), SecretError> {
        no_ui()?;
        match delete_generic_password(&self.service, account(key)?) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ERR_ITEM_NOT_FOUND => Ok(()),
            Err(e) if e.code() == ERR_INVALID_OWNER_EDIT => {
                set_generic_password(&self.service, account(key)?, TOMBSTONE).map_err(map_err)
            }
            Err(e) => Err(map_err(e)),
        }
    }
}
