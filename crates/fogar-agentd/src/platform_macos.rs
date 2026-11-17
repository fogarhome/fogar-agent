//! macOS wiring. Keychain secrets and the UI socket are real (S1); audio is a stub, the probe and
//! power policy partial.

use fogar_agent_macos::{MacAudioSink, MacAudioSource, MacIpcServer, MacKeychainStore, MacPowerPolicy, MacSystemProbe, MacTailnet};
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

pub(crate) fn secret_store(_state_dir: &Path) -> std::io::Result<Arc<dyn SecretStore>> {
    Ok(Arc::new(MacKeychainStore::new()))
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
