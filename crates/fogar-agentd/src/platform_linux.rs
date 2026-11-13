//! Linux wiring (illustrative adapters; see fogar-agent-linux).

use fogar_agent_linux::{FileSecretStore, LinuxPowerPolicy, NoAudioSink, NoAudioSource, ProcSystemProbe, UdsIpcServer};
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
    pub(crate) ipc: Option<UdsIpcServer>,
}

pub(crate) fn default_state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(d).join("fogar-agent");
    }
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp/fogar-agent"), |h| PathBuf::from(h).join(".local/state/fogar-agent"))
}

pub(crate) fn secret_store(state_dir: &Path) -> std::io::Result<Arc<dyn SecretStore>> {
    Ok(Arc::new(FileSecretStore::open(&state_dir.join("secrets"))?))
}

pub(crate) fn power_policy() -> Arc<dyn PowerPolicy> {
    Arc::new(LinuxPowerPolicy)
}

/// Must run inside the tokio runtime (the IPC listener registers with the reactor).
pub(crate) fn build(state_dir: &Path, with_ipc: bool) -> std::io::Result<Platform> {
    let ipc = if with_ipc { Some(UdsIpcServer::bind(&state_dir.join("run"))?) } else { None };
    Ok(Platform {
        secrets: secret_store(state_dir)?,
        audio_in: Arc::new(NoAudioSource),
        audio_out: Arc::new(NoAudioSink),
        probe: Arc::new(ProcSystemProbe),
        ipc,
    })
}
