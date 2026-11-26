//! S4 end-to-end (the file keeps its S1 name: CI runs it by name): the whole account lifecycle
//! over the REAL transport. Real HTTP (`fogar_host::HttpGateway`) and real WebSocket
//! (`WsGatewayLink`), against two gateways:
//! - the Rust mock (`MockGateway`), which serves `/auth/*` and `/ws/stream` on one loopback port
//!   like the real one: always runs;
//! - the REAL fogar-gateway (Python) with its own fake OpenClaw and no STT/TTS, started by
//!   `real_gateway/launch.py` from the checkout in `FOGAR_GATEWAY_DIR` (skipped when unset; CI
//!   job `.github/workflows/e2e-gateway.yml` checks out the commit in `real_gateway/GATEWAY_REF`).
//!
//! Loopback is one of the addresses option B allows for a password over plain HTTP.
//! The app side of the IPC is the in-memory `FakeIpc` (the real socket's code-signing check
//! cannot accept a test binary). Real sockets, so real time, not paused time.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use fogar_core::{Agent, AgentPorts, Config, HealthProbe, LinkState, crashloop::BootMode};
use fogar_host::{HttpConfig, HttpGateway, TokioClock, WsConfig, WsGatewayLink};
use fogar_ports::{
    ipc::{GatewayInfo, IpcRequest, IpcResponse, LinkPhase, LoginForm, Password},
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

/// Memory store that counts deletions of the account.
#[derive(Debug, Default)]
struct CountingStore {
    inner: MemorySecretStore,
    account_deletes: AtomicU32,
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
        if key == SecretKey::GatewayAccount && self.inner.contains(key) {
            self.account_deletes.fetch_add(1, Ordering::SeqCst);
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

/// The gateway's `201` for `POST /auth/login`, handing out `token`.
fn login_ok(token: &str) -> String {
    format!(
        r#"{{"client_id":"c-1","token":"{token}","scopes":["conversations:read","conversations:write"],"user":{{"id":"u-1","username":"sito","display_name":"Sito","role":"member"}},"home":{{"id":"home","name":"mock"}}}}"#
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

async fn login(c: &mut FakeIpcClient, address: &str, user: &str, password: &str) -> GatewayInfo {
    let r = c
        .request(IpcRequest::Login(Box::new(LoginForm {
            address: address.into(),
            username: user.into(),
            password: Password::parse(password),
        })))
        .await;
    match r {
        Some(IpcResponse::LoggedIn(info)) => *info,
        other => panic!("{other:?}"),
    }
}

async fn logout(c: &mut FakeIpcClient) -> bool {
    match c.request(IpcRequest::Logout).await {
        Some(IpcResponse::LoggedOut { gateway_reached }) => gateway_reached,
        other => panic!("{other:?}"),
    }
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

/// A signed-out agent on the real adapters, with a counting store and a signed fake IPC.
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

/// signed out -> login -> ready -> gateway restart (1012) -> ready again -> revoked (4200) ->
/// account deleted once, signed out, no retry -> login again with a new token -> ready ->
/// logout (gateway told with the token, account deleted) -> signed out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_lifecycle_against_the_mock_gateway() {
    let mock = MockConfig {
        client_key: "key-1".into(),
        ..MockConfig::default()
    };
    let gw = MockGateway::start(mock.clone()).await.unwrap();
    gw.routes()
        .set("POST", "/auth/login", 201, login_ok("key-1"));
    gw.routes().set("POST", "/auth/logout", 204, "");
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

    // Sign in over HTTP, then the link comes up over WebSocket with the token it handed out.
    let info = login(&mut app, &gw.addr().to_string(), " sito ", "contraseña-1").await;
    assert_eq!((info.user.as_str(), info.name.as_str()), ("Sito", "mock"));
    ready(&mut h, 1).await;
    let post = gw
        .routes()
        .seen()
        .into_iter()
        .find(|r| r.method == "POST")
        .unwrap();
    assert_eq!(post.path, "/auth/login");
    assert_eq!(post.authorization, None);
    let body = String::from_utf8_lossy(&post.body);
    assert!(
        body.contains(r#""username":"sito","password":"contraseña-1""#),
        "{body}"
    );
    assert_eq!(gw.log().handshakes_ok, 1);
    assert_eq!(
        gw.log().connections,
        1,
        "the REST calls are not WS sessions"
    );

    // Gateway restart: reconnects by itself, keeps the token.
    assert_eq!(gw.close_all(1012, ""), 1);
    ready(&mut h, 2).await;
    assert_eq!(secrets.account_deletes.load(Ordering::SeqCst), 0);
    assert_eq!(gw.log().handshakes_ok, 2);

    // The device is removed (web or admin). From now on the gateway only knows the next token,
    // so a retry with the old one would be refused (and delete again).
    gw.set_config(MockConfig {
        client_key: "key-2".into(),
        ..mock
    });
    gw.routes()
        .set("POST", "/auth/login", 201, login_ok("key-2"));
    assert_eq!(gw.close_all(4200, "credential revoked"), 1);
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
    assert_eq!(secrets.account_deletes.load(Ordering::SeqCst), 1);
    assert!(!secrets.inner.contains(SecretKey::GatewayAccount));

    // Signing in again works and the link comes up with the new token.
    login(&mut app, &gw.addr().to_string(), "sito", "contraseña-1").await;
    ready(&mut h, 3).await;
    assert_eq!(gw.log().handshakes_ok, 3);

    // Logout: the gateway hears it with the token, the account is gone, no reconnect.
    assert!(logout(&mut app).await);
    let out = gw.routes().seen().into_iter().last().unwrap();
    assert_eq!(
        (out.path.as_str(), out.authorization.as_deref()),
        ("/auth/logout", Some("Bearer key-2"))
    );
    assert_eq!(secrets.account_deletes.load(Ordering::SeqCst), 2);
    within(
        Duration::from_secs(10),
        h.wait_link(|v| v.state == LinkState::NotPaired),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(gw.log().connections, 3, "no reconnect after logout");

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

    // A member the admin created; sign in over plain HTTP to loopback, then the session comes up.
    let creds = gw.cmd("user", "user", Duration::from_secs(10)).await;
    let (user, password) = creds.split_once(' ').unwrap();
    let info = login(&mut app, &gw.addr, user, password).await;
    assert_eq!(info.user, "Sito");
    assert!(!info.name.is_empty(), "the home's name: {info:?}");
    ready(&mut h, 1).await;

    // Restart (uvicorn closes with 1012): reconnects by itself once it is back, same token.
    gw.cmd("restart", "restarted", Duration::from_secs(180))
        .await;
    let v = within(
        Duration::from_secs(30),
        h.wait_link(|v| matches!(v.state, LinkState::Ready { session } if session > 1)),
    )
    .await;
    assert_eq!(v.last_close.map(|c| c.0), Some(1012));
    assert_eq!(secrets.account_deletes.load(Ordering::SeqCst), 0);

    // The admin removes the device: 4200 on the live session, account deleted once, no retry.
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
    assert_eq!(secrets.account_deletes.load(Ordering::SeqCst), 1);

    // Signing in again makes a new device.
    login(&mut app, &gw.addr, user, password).await;
    within(
        Duration::from_secs(10),
        h.wait_link(|v| matches!(v.state, LinkState::Ready { .. })),
    )
    .await;

    // Logout: the gateway accepts the token (204) and retires the device; the account is gone.
    // The gateway also closes the live session (4200), which may delete the account first.
    assert!(logout(&mut app).await, "the gateway answered the logout");
    assert!(!secrets.inner.contains(SecretKey::GatewayAccount));
    within(
        Duration::from_secs(10),
        h.wait_link(|v| v.state == LinkState::NotPaired),
    )
    .await;
    assert_eq!(
        gw.cmd("devices", "devices", Duration::from_secs(10)).await,
        "0"
    );

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
