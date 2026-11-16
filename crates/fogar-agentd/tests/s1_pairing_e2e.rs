//! S1 end-to-end: the whole pairing lifecycle over the REAL transport. Real HTTP
//! (`fogar_host::HttpGateway`) and real WebSocket (`WsGatewayLink`) against the mock gateway,
//! which serves `/pair` and `/ws/stream` on one loopback port like the real one. The app side of
//! the IPC is the in-memory `FakeIpc` (the real socket's code-signing check cannot accept a test
//! binary). Real sockets, so real time (a few seconds), not paused time.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use fogar_core::{Agent, AgentPorts, Config, HealthProbe, LinkState, crashloop::BootMode};
use fogar_host::{HttpConfig, HttpGateway, TokioClock, WsConfig, WsGatewayLink};
use fogar_ports::{
    ipc::{
        IpcCaps, IpcError, IpcRequest, IpcResponse, IpcServer, LinkPhase, PairCode, PeerIdentity,
    },
    secrets::{SecretCaps, SecretError, SecretKey, SecretStore, SecretString},
};
use fogar_testkit::{
    FakeIpc, FakeIpcClient, FakeMic, FakeSpeaker, MemorySecretStore, MockConfig, MockGateway,
    SeededRandom,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

/// `FakeIpc` whose peers count as the same signer as the agent (IPC-4 lets them pair).
#[derive(Debug, Clone)]
struct SignedIpc(FakeIpc);

impl IpcServer for SignedIpc {
    type Conn = <FakeIpc as IpcServer>::Conn;
    fn capabilities(&self) -> IpcCaps {
        self.0.capabilities()
    }
    async fn accept(&self) -> Result<(PeerIdentity, Self::Conn), IpcError> {
        let (peer, conn) = self.0.accept().await?;
        Ok((
            PeerIdentity::verified_signer(peer.uid, "com.fogar.desktop".into()),
            conn,
        ))
    }
}

/// Memory store that counts deletions of the pairing record.
#[derive(Debug, Default)]
struct CountingStore {
    inner: MemorySecretStore,
    pairing_deletes: AtomicU32,
}

impl SecretStore for CountingStore {
    fn capabilities(&self) -> SecretCaps {
        self.inner.capabilities()
    }
    fn get(&self, key: SecretKey) -> Result<Option<SecretString>, SecretError> {
        self.inner.get(key)
    }
    fn put(&self, key: SecretKey, value: SecretString) -> Result<(), SecretError> {
        self.inner.put(key, value)
    }
    fn delete(&self, key: SecretKey) -> Result<(), SecretError> {
        if key == SecretKey::GatewayPairing {
            self.pairing_deletes.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.delete(key)
    }
}

fn fast_config() -> Config {
    let mut c = Config::default();
    c.backoff.base = Duration::from_millis(20);
    c.backoff.cap = Duration::from_millis(200);
    c.backoff.floor = Duration::from_millis(5);
    c
}

/// The gateway's `201` for `POST /pair` (spec §0), handing out `key`.
fn pair_ok(key: &str) -> String {
    format!(
        r#"{{"client_id":"c-1","name":"Mac de Sito","client_key":"{key}","gateway":{{"name":"mock","version":"2.0.0"}}}}"#
    )
}

async fn within<F: std::future::Future>(d: Duration, f: F) -> F::Output {
    tokio::time::timeout(d, f).await.expect("timed out")
}

async fn ready(h: &mut HealthProbe, session: u64) {
    within(
        Duration::from_secs(10),
        h.wait_link(|v| v.state == LinkState::Ready { session }),
    )
    .await;
}

async fn pair(c: &mut FakeIpcClient, gw: &MockGateway, code: &str) {
    let r = c
        .request(IpcRequest::Pair {
            address: gw.addr().to_string(),
            code: PairCode::parse(code),
        })
        .await;
    assert!(matches!(r, Some(IpcResponse::Paired(_))), "{r:?}");
}

async fn phase(c: &mut FakeIpcClient) -> LinkPhase {
    match c.request(IpcRequest::Status).await {
        Some(IpcResponse::Status(s)) => s.phase,
        other => panic!("expected Status, got {other:?}"),
    }
}

fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

/// unpaired -> pair -> ready -> gateway restart (1012) -> ready again -> revoked -> key deleted
/// once, not_paired, no retry -> pair again with a new key -> ready.
async fn lifecycle(m19: bool, revoke: (u16, &str)) {
    let mock = MockConfig {
        client_key: "key-1".into(),
        m19,
        ..MockConfig::default()
    };
    let gw = MockGateway::start(mock.clone()).await.unwrap();
    gw.routes().set("POST", "/pair", 201, pair_ok("key-1"));
    let baseline = alive_tasks();
    let secrets = Arc::new(CountingStore::default());
    let ipc = FakeIpc::new();
    let ports = AgentPorts {
        clock: Arc::new(TokioClock::new()),
        link: WsGatewayLink::new(WsConfig::default()),
        ipc: Some(SignedIpc(ipc.clone())),
        http: HttpGateway::new(HttpConfig::default()),
        secrets: secrets.clone(),
        random: Arc::new(SeededRandom::new(11)),
        audio_in: Arc::new(FakeMic::minimal()),
        audio_out: Arc::new(FakeSpeaker::minimal()),
        probe: None,
    };
    let agent = Agent::start(ports, fast_config(), BootMode::Normal);
    let mut h = agent.health();
    within(
        Duration::from_secs(10),
        h.wait_link(|v| v.state == LinkState::NotPaired),
    )
    .await;
    let mut app = ipc.client().await;
    assert_eq!(phase(&mut app).await, LinkPhase::NotPaired);

    // Pair over HTTP, then the link comes up over WebSocket with the key /pair handed out.
    pair(&mut app, &gw, "482 913").await;
    ready(&mut h, 1).await;
    let post = gw
        .routes()
        .seen()
        .into_iter()
        .find(|r| r.method == "POST")
        .unwrap();
    assert_eq!(post.path, "/pair");
    let body = String::from_utf8_lossy(&post.body);
    assert!(body.contains(r#""code":"482913""#), "{body}");
    assert_eq!(gw.log().handshakes_ok, 1);
    assert_eq!(
        gw.log().connections,
        1,
        "the REST calls are not WS sessions"
    );

    // Gateway restart: reconnects by itself, keeps the key.
    assert_eq!(gw.close_all(1012, ""), 1);
    ready(&mut h, 2).await;
    assert_eq!(secrets.pairing_deletes.load(Ordering::SeqCst), 0);
    assert_eq!(gw.log().handshakes_ok, 2);

    // The admin revokes the key. From now on the gateway only knows the next one, so a retry
    // with the old key would be refused (and delete again).
    gw.set_config(MockConfig {
        client_key: "key-2".into(),
        ..mock
    });
    gw.routes().set("POST", "/pair", 201, pair_ok("key-2"));
    assert_eq!(gw.close_all(revoke.0, revoke.1), 1);
    within(
        Duration::from_secs(10),
        h.wait_link(|v| v.state == LinkState::NotPaired),
    )
    .await;
    assert_eq!(phase(&mut app).await, LinkPhase::NotPaired);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let log = gw.log();
    assert_eq!(log.connections, 2, "no reconnect after revocation");
    assert_eq!(log.handshakes_rejected, 0);
    assert_eq!(secrets.pairing_deletes.load(Ordering::SeqCst), 1);
    assert!(!secrets.inner.contains(SecretKey::GatewayPairing));

    // Pairing again works and the link comes up with the new key.
    pair(&mut app, &gw, "105 772").await;
    within(
        Duration::from_secs(10),
        h.wait_link(|v| matches!(v.state, LinkState::Ready { .. })),
    )
    .await;
    assert_eq!(phase(&mut app).await, LinkPhase::Ready);
    assert_eq!(gw.log().handshakes_ok, 3);
    assert_eq!(secrets.pairing_deletes.load(Ordering::SeqCst), 1);

    drop(app);
    assert!(agent.shutdown(Duration::from_secs(5)).await.clean);
    within(Duration::from_secs(5), async {
        while alive_tasks() != baseline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairing_lifecycle_v3_gateway_revokes_with_4200() {
    lifecycle(true, (4200, "credential revoked")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairing_lifecycle_v2_gateway_revokes_with_1008() {
    lifecycle(false, (1008, "revoked")).await;
}
