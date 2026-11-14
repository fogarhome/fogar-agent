//! The macOS adapters against the shared contract suites (macOS CI runner and the M2).
#![cfg(target_os = "macos")]

use fogar_agent_macos::*;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
};

fn unique(tag: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    format!(
        "fogar-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

fn scratch_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(unique(tag))
}

/// Never the real `com.fogar.agent` items; one service per clause (they run in parallel).
fn test_keychain() -> MacKeychainStore {
    MacKeychainStore::with_service(&format!("com.fogar.agent.test.{}", unique("kc")))
}

struct Client(MacIpcClient);

impl fogar_testkit::contract::ipc_server::IpcTestClient for Client {
    async fn request(
        &mut self,
        r: fogar_ports::ipc::IpcRequest,
    ) -> Option<fogar_ports::ipc::IpcResponse> {
        self.0.request(r).await
    }
    async fn next(&mut self) -> Option<fogar_ports::ipc::IpcResponse> {
        self.0.next().await
    }
    async fn send_half(&mut self, r: &fogar_ports::ipc::IpcRequest, first: bool) {
        assert!(
            self.0.send_half(r, first).await.is_some(),
            "split write failed"
        );
    }
}

fogar_testkit::system_probe_contract!(MacSystemProbe);
fogar_testkit::secret_store_contract!(test_keychain());
fogar_testkit::audio_source_contract!(MacAudioSource);
fogar_testkit::audio_sink_contract!(MacAudioSink);
fogar_testkit::power_policy_contract!(MacPowerPolicy);
fogar_testkit::ipc_server_contract!({
    let server = MacIpcServer::bind(&scratch_dir("ipc")).unwrap();
    let path = server.path().to_path_buf();
    (server, move || async move {
        Client(MacIpcClient::connect(&path).await.unwrap())
    })
});

#[test]
fn keychain_tombstone_reads_as_absent() {
    use fogar_ports::secrets::{SecretKey, SecretStore};
    let s = test_keychain();
    let k = SecretKey::GatewayPairing;
    s.put(k, "record".to_owned().into()).unwrap();
    // What `delete` writes when the item ACL forbids removing it (a rebuilt agent): absent, and
    // `put` can never store it by accident.
    security_framework::passwords::set_generic_password(s.service(), "gateway_pairing", b" ")
        .unwrap();
    assert!(s.get(k).unwrap().is_none());
    assert!(s.put(k, " ".to_owned().into()).is_err());
    s.put(k, "again".to_owned().into()).unwrap();
    assert!(s.get(k).unwrap().is_some());
    s.delete(k).unwrap();
    assert!(s.get(k).unwrap().is_none());
}

#[test]
fn ipc_reports_peer_check_honestly() {
    use fogar_ports::{
        Support,
        ipc::{IpcRequest, IpcServer, PeerCheck},
    };
    fogar_testkit::run_async(async {
        let server = MacIpcServer::bind(&scratch_dir("caps")).unwrap();
        let caps = server.capabilities();
        assert_eq!(caps.peer_uid, Support::Native);
        let path = server.path().to_path_buf();
        let client = async {
            let mut c = MacIpcClient::connect(&path).await.unwrap();
            c.request(IpcRequest::Status).await
        };
        let serve = async {
            let (peer, _conn) = server.accept().await.unwrap();
            peer
        };
        let (peer, _) = tokio::join!(serve, client);
        let me = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(server.path()).unwrap().uid()
        };
        assert_eq!(peer.uid, Some(me));
        // The client is this test binary: never the companion app id, signed or not. Signed
        // agents and peers are exercised by examples/signer_probe.rs (results in ADR-0002).
        assert_eq!(peer.check(), PeerCheck::UidOnly);
        assert_eq!(peer.code_id(), None);
    });
}

#[test]
fn stale_socket_is_replaced_but_live_one_is_not() {
    fogar_testkit::run_async(async {
        let dir = scratch_dir("live");
        let live = MacIpcServer::bind(&dir).unwrap();
        assert!(
            MacIpcServer::bind(&dir).is_err(),
            "a live agent owns the socket"
        );
        drop(live);
        assert!(
            !dir.join("agent.sock").exists(),
            "socket file removed on drop"
        );

        let stale = scratch_dir("stale");
        ensure_private_dir(&stale).unwrap();
        drop(std::os::unix::net::UnixListener::bind(stale.join("agent.sock")).unwrap());
        assert!(MacIpcServer::bind(&stale).is_ok());

        let occupied = scratch_dir("occupied");
        ensure_private_dir(&occupied).unwrap();
        std::fs::write(occupied.join("agent.sock"), b"not a socket").unwrap();
        assert!(MacIpcServer::bind(&occupied).is_err());
        assert!(occupied.join("agent.sock").exists());
    });
}

#[test]
fn socket_is_private_and_long_paths_are_refused() {
    use std::os::unix::fs::PermissionsExt;
    fogar_testkit::run_async(async {
        let dir = scratch_dir("perm");
        let s = MacIpcServer::bind(&dir).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(s.path()), 0o600);
        let long = scratch_dir(&"x".repeat(100));
        assert!(MacIpcServer::bind(&long).is_err());
    });
}
