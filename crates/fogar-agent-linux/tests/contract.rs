//! The Linux adapters pass the same contract suites as the fakes.
#![cfg(target_os = "linux")]

use fogar_agent_linux::*;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
};

fn scratch_dir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!(
        "fogar-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

struct Client(UdsIpcClient);

impl fogar_testkit::contract::ipc_server::IpcTestClient for Client {
    async fn request(
        &mut self,
        r: fogar_ports::ipc::IpcRequest,
    ) -> Option<fogar_ports::ipc::IpcResponse> {
        self.0.request(r).await
    }
}

fogar_testkit::system_probe_contract!(ProcSystemProbe);
fogar_testkit::secret_store_contract!(FileSecretStore::open(&scratch_dir("secrets")).unwrap());
fogar_testkit::audio_source_contract!(NoAudioSource);
fogar_testkit::audio_sink_contract!(NoAudioSink);
fogar_testkit::power_policy_contract!(LinuxPowerPolicy);
fogar_testkit::ipc_server_contract!({
    let server = UdsIpcServer::bind(&scratch_dir("ipc")).unwrap();
    let path = server.path().to_path_buf();
    (server, move || async move {
        Client(UdsIpcClient::connect(&path).await.unwrap())
    })
});

#[test]
fn secret_file_with_loose_permissions_is_refused() {
    use fogar_ports::secrets::{SecretKey, SecretStore};
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch_dir("loose");
    let s = FileSecretStore::open(&dir).unwrap();
    s.put(SecretKey::GatewayClientKey, "k".to_owned().into())
        .unwrap();
    let f = dir.join("gateway_client_key");
    assert_eq!(
        std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        s.get(SecretKey::GatewayClientKey).is_err(),
        "world-readable key must not be used"
    );
}

#[test]
fn stale_socket_is_replaced_but_live_one_is_not() {
    fogar_testkit::run_async(async {
        let dir = scratch_dir("live");
        let live = UdsIpcServer::bind(&dir).unwrap();
        assert!(
            UdsIpcServer::bind(&dir).is_err(),
            "a live agent owns the socket"
        );
        drop(live);
        assert!(
            !dir.join("agent.sock").exists(),
            "socket file removed on drop"
        );

        // A crashed agent leaves the file behind with nobody listening.
        let stale = scratch_dir("stale");
        ensure_private_dir(&stale).unwrap();
        drop(std::os::unix::net::UnixListener::bind(stale.join("agent.sock")).unwrap());
        assert!(stale.join("agent.sock").exists());
        assert!(
            UdsIpcServer::bind(&stale).is_ok(),
            "stale socket from a dead process is replaced"
        );

        // A non-socket file at the path is never deleted.
        let occupied = scratch_dir("occupied");
        ensure_private_dir(&occupied).unwrap();
        std::fs::write(occupied.join("agent.sock"), b"not a socket").unwrap();
        assert!(UdsIpcServer::bind(&occupied).is_err());
        assert!(occupied.join("agent.sock").exists());
    });
}
