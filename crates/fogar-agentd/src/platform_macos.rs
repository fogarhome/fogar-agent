//! macOS wiring. Keychain secrets and the UI socket are real (S1); audio is the real
//! cpal/CoreAudio adapter (S2: the engine opens the microphone only through its capture gate, from
//! S2 PR 3b); the probe and power policy are partial.

use fogar_agent_macos::{
    KEYCHAIN_SERVICE, MacAudioSink, MacAudioSource, MacIpcServer, MacKeychainStore,
    MacPowerPolicy, MacSystemProbe, MacTailnet,
};
use fogar_ports::{
    audio::{AudioSink, AudioSource},
    power::PowerPolicy,
    secrets::SecretStore,
    system_probe::SystemProbe,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) struct Platform {
    pub(crate) secrets: Arc<dyn SecretStore>,
    pub(crate) audio_in: Arc<dyn AudioSource>,
    pub(crate) audio_out: Arc<dyn AudioSink>,
    pub(crate) probe: Arc<dyn SystemProbe>,
    pub(crate) ipc: Option<MacIpcServer>,
    pub(crate) tailnet: MacTailnet,
}

/// Frozen identity (fogar-macos-daemon hard rule 2): never change across versions.
/// `None` when `HOME` is unset or relative: no predictable `/tmp` fallback (review sec#1).
pub(crate) fn default_state_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute())
        .map(|h| h.join("Library/Application Support/com.fogar.agent"))
}

/// The default state directory keeps the frozen Keychain service; an explicit `--state-dir`
/// elsewhere gets its own, derived from the path (`instance::keychain_service`), so agents run
/// side by side for measurements never share or overwrite one pairing.
pub(crate) fn secret_store(state_dir: &Path) -> std::io::Result<Arc<dyn SecretStore>> {
    Ok(Arc::new(keychain_store(state_dir, default_state_dir().as_deref())))
}

fn keychain_store(state_dir: &Path, default: Option<&Path>) -> MacKeychainStore {
    let service = crate::instance::keychain_service(KEYCHAIN_SERVICE, state_dir, default);
    MacKeychainStore::with_service(&service)
}

pub(crate) fn power_policy() -> Arc<dyn PowerPolicy> {
    Arc::new(MacPowerPolicy)
}

pub(crate) fn build(state_dir: &Path, with_ipc: bool) -> std::io::Result<Platform> {
    let ipc = if with_ipc { Some(MacIpcServer::bind(&state_dir.join("run"))?) } else { None };
    Ok(Platform {
        secrets: secret_store(state_dir)?,
        audio_in: Arc::new(MacAudioSource),
        audio_out: Arc::new(MacAudioSink),
        probe: Arc::new(MacSystemProbe),
        ipc,
        tailnet: MacTailnet::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default directory keeps the frozen item; an explicit one has its own, stable service.
    #[test]
    fn explicit_state_dir_has_its_own_keychain_service() {
        let default = Path::new("/Users/x/Library/Application Support/com.fogar.agent");
        assert_eq!(keychain_store(default, Some(default)).service(), KEYCHAIN_SERVICE);
        let a = keychain_store(Path::new("/tmp/fogar-a"), Some(default));
        assert_eq!(a.service(), "com.fogar.agent.dir-38520729324438e5");
        assert_ne!(
            a.service(),
            keychain_store(Path::new("/tmp/fogar-b"), Some(default)).service()
        );
    }
}
