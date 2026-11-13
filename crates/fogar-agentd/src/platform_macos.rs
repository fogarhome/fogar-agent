//! macOS wiring — UNTESTED (never compiled in slice 0). All adapters are stubs except load
//! average and thread QoS; with the Keychain stub the agent stays `NotPaired` (no loop).

use fogar_agent_macos::{MacAudioSink, MacAudioSource, MacIpcServer, MacKeychainStore, MacPowerPolicy, MacSystemProbe};
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
}

/// Frozen identity (fogar-macos-daemon hard rule 2): never change across versions.
pub(crate) fn default_state_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from("/tmp/com.fogar.agent"),
        |h| PathBuf::from(h).join("Library/Application Support/com.fogar.agent"),
    )
}

pub(crate) fn secret_store(_state_dir: &Path) -> std::io::Result<Arc<dyn SecretStore>> {
    Ok(Arc::new(MacKeychainStore))
}

pub(crate) fn power_policy() -> Arc<dyn PowerPolicy> {
    Arc::new(MacPowerPolicy)
}

pub(crate) fn build(state_dir: &Path, with_ipc: bool) -> std::io::Result<Platform> {
    Ok(Platform {
        secrets: secret_store(state_dir)?,
        audio_in: Arc::new(MacAudioSource),
        audio_out: Arc::new(MacAudioSink),
        probe: Arc::new(MacSystemProbe),
        ipc: with_ipc.then_some(MacIpcServer),
    })
}
