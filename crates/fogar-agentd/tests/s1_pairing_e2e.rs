//! S1 end-to-end: the whole pairing lifecycle over the REAL transport. Real HTTP
//! (`fogar_host::HttpGateway`) and real WebSocket (`WsGatewayLink`), against two gateways:
//! - the Rust mock (`MockGateway`), which serves `/pair` and `/ws/stream` on one loopback port
//!   like the real one: always runs, covers v3 (4200) and 2.x (1008 "revoked");
//! - the REAL fogar-gateway (Python) with its own fake OpenClaw and no STT/TTS, started by
//!   `real_gateway/launch.py` from the checkout in `FOGAR_GATEWAY_DIR` (skipped when unset; CI
//!   job `.github/workflows/e2e-gateway.yml` checks out the commit in `real_gateway/GATEWAY_REF`).
//!
//! The app side of the IPC is the in-memory `FakeIpc` (the real socket's code-signing check
//! cannot accept a test binary). Real sockets, so real time, not paused time.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use fogar_core::{Agent, AgentPorts, Config, HealthProbe, LinkState, crashloop::BootMode};
use fogar_host::{HttpConfig, HttpGateway, TokioClock, WsConfig, WsGatewayLink};
use fogar_ports::{
    ipc::{IpcRequest, IpcResponse, LinkPhase, PairCode},
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

async fn pair(c: &mut FakeIpcClient, address: &str, code: &str) {
    let r = c
        .request(IpcRequest::Pair {
            address: address.into(),
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

/// An unpaired agent on the real adapters, with a counting store and a signed fake IPC.
fn start_agent() -> (Agent<TokioClock>, Arc<CountingStore>, FakeIpc) {
    let secrets = Arc::new(CountingStore::default());
    let ipc = FakeIpc::signed();
    let ports = AgentPorts {
        clock: Arc::new(TokioClock::new()),
        link: WsGatewayLink::new(WsConfig::default()),
        ipc: Some(ipc.clone()),
        http: HttpGateway::new(HttpConfig::default()),
        tailnet: fogar_core::NoTailnet,
        secrets: secrets.clone(),
        random: Arc::new(SeededRandom::new(11)),
        audio_in: Arc::new(FakeMic::minimal()),
        audio_out: Arc::new(FakeSpeaker::minimal()),
        probe: None,
    };
    let agent = Agent::start(ports, fast_config(), BootMode::Normal);
    (agent, secrets, ipc)
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
    let (agent, secrets, ipc) = start_agent();
    let mut h = agent.health();
    within(
        Duration::from_secs(10),
        h.wait_link(|v| v.state == LinkState::NotPaired),
    )
    .await;
    let mut app = ipc.client().await;
    assert_eq!(phase(&mut app).await, LinkPhase::NotPaired);

    // Pair over HTTP, then the link comes up over WebSocket with the key /pair handed out.
    pair(&mut app, &gw.addr().to_string(), "482 913").await;
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
    pair(&mut app, &gw.addr().to_string(), "105 772").await;
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

/// The real gateway, driven over `launch.py`'s line protocol. Dropping it closes stdin, which
/// stops the gateway and its fakes.
struct RealGateway {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    out: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    addr: String,
}

impl RealGateway {
    /// `None` when `FOGAR_GATEWAY_DIR` is unset.
    async fn start() -> Option<Self> {
        use tokio::io::AsyncBufReadExt as _;
        let dir = std::env::var_os("FOGAR_GATEWAY_DIR")?;
        let launch =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/real_gateway/launch.py");
        let mut child = tokio::process::Command::new("uv")
            .args(["run", "--locked", "python"])
            .arg(launch)
            .current_dir(&dir)
            .env("PYTHONPATH", &dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("uv (https://docs.astral.sh/uv/) runs the gateway");
        let stdin = child.stdin.take().unwrap();
        let out = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let mut gw = Self {
            child,
            stdin,
            out,
            addr: String::new(),
        };
        let port = gw.line("ready", Duration::from_secs(180)).await;
        gw.addr = format!("127.0.0.1:{port}");
        Some(gw)
    }

    /// Next stdout line, which must start with `word`; returns the rest.
    async fn line(&mut self, word: &str, d: Duration) -> String {
        let l = within(d, self.out.next_line())
            .await
            .unwrap()
            .expect("launcher exited");
        let rest = l
            .strip_prefix(word)
            .unwrap_or_else(|| panic!("expected {word}, got {l:?}"));
        rest.trim().to_owned()
    }

    async fn cmd(&mut self, cmd: &str, reply: &str, d: Duration) -> String {
        use tokio::io::AsyncWriteExt as _;
        self.stdin
            .write_all(format!("{cmd}\n").as_bytes())
            .await
            .unwrap();
        self.stdin.flush().await.unwrap();
        self.line(reply, d).await
    }

    async fn stop(self) {
        let Self {
            mut child, stdin, ..
        } = self;
        drop(stdin);
        within(Duration::from_secs(30), child.wait()).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairing_lifecycle_against_the_real_gateway() {
    let Some(mut gw) = RealGateway::start().await else {
        eprintln!("FOGAR_GATEWAY_DIR not set: real-gateway E2E skipped");
        return;
    };
    let baseline = alive_tasks();
    let (agent, secrets, ipc) = start_agent();
    let mut h = agent.health();
    within(
        Duration::from_secs(10),
        h.wait_link(|v| v.state == LinkState::NotPaired),
    )
    .await;
    let mut app = ipc.client().await;

    // Pair with a code the admin opened, then the session comes up.
    let code = gw.cmd("code", "code", Duration::from_secs(10)).await;
    pair(&mut app, &gw.addr, &code).await;
    ready(&mut h, 1).await;

    // Restart (uvicorn closes with 1012): reconnects by itself once it is back, same key.
    gw.cmd("restart", "restarted", Duration::from_secs(180))
        .await;
    let v = within(
        Duration::from_secs(30),
        h.wait_link(|v| matches!(v.state, LinkState::Ready { session } if session > 1)),
    )
    .await;
    assert_eq!(v.last_close.map(|c| c.0), Some(1012));
    assert_eq!(secrets.pairing_deletes.load(Ordering::SeqCst), 0);

    // The admin deletes the client: 4200 on the live session, key deleted once, no retry.
    assert_eq!(
        gw.cmd("revoke", "revoked", Duration::from_secs(10)).await,
        "1"
    );
    let v = within(
        Duration::from_secs(10),
        h.wait_link(|v| v.state == LinkState::NotPaired),
    )
    .await;
    assert_eq!(v.last_close.map(|c| c.0), Some(4200));
    let attempts = h.snapshot().stats.connect_attempts;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(phase(&mut app).await, LinkPhase::NotPaired);
    assert_eq!(
        h.snapshot().stats.connect_attempts,
        attempts,
        "no reconnect after revocation"
    );
    assert_eq!(secrets.pairing_deletes.load(Ordering::SeqCst), 1);

    // A new code pairs again.
    let code = gw.cmd("code", "code", Duration::from_secs(10)).await;
    pair(&mut app, &gw.addr, &code).await;
    within(
        Duration::from_secs(10),
        h.wait_link(|v| matches!(v.state, LinkState::Ready { .. })),
    )
    .await;
    assert_eq!(secrets.pairing_deletes.load(Ordering::SeqCst), 1);

    drop(app);
    assert!(agent.shutdown(Duration::from_secs(5)).await.clean);
    within(Duration::from_secs(5), async {
        while alive_tasks() != baseline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    gw.stop().await;
}
