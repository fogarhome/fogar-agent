//! S4 end-to-end (the file keeps its S1 name: CI runs it by name): the whole account lifecycle
//! over the REAL transport (`fogar_host::HttpGateway`: `/auth/*` and the `GET /events` link),
//! against two gateways:
//! - the Rust mock (`MockGateway` + `EventsHub`), which serves `/auth/*` and `/events` on one
//!   loopback port like the real one: always runs;
//! - the REAL fogar-gateway (Python) with its own fake OpenClaw and no STT/TTS (which answers a
//!   text, so the chat over `/events` is checked there too: send, reply, list, reopen), started by
//!   `real_gateway/launch.py` from the checkout in `FOGAR_GATEWAY_DIR` (skipped when unset; CI
//!   job `.github/workflows/e2e-gateway.yml` checks out the commit in `real_gateway/GATEWAY_REF`).
//!
//! Loopback is one of the addresses option B allows for a password over plain HTTP.
//! The app side of the IPC is the in-memory `FakeIpc` (the real socket's code-signing check
//! cannot accept a test binary). Real sockets, so real time, not paused time.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::*;
use fogar_core::LinkState;
use fogar_ports::{
    ipc::{IpcRequest, IpcResponse, LinkPhase, MessageState, TurnOutcome, ViewEntry},
    secrets::SecretKey,
};
use fogar_testkit::{EventsHub, MockConfig, MockGateway};
use std::{sync::atomic::Ordering, time::Duration};

/// signed out -> login -> ready -> stream lost -> ready again -> revoked (401) -> account deleted
/// once, signed out, no retry -> login again with a new token -> ready -> logout (gateway told
/// with the token, account deleted) -> signed out. The chat link is `GET /events` (S4 PR 4a):
/// the mock serves it from `EventsHub` on the same port.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_lifecycle_against_the_mock_gateway() {
    let gw = MockGateway::start(MockConfig::default()).await.unwrap();
    let hub = EventsHub::new();
    hub.install(&gw.routes());
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

    // Sign in over HTTP, then the link comes up over `/events` with the token it handed out.
    let info = login(&mut app, &gw.addr().to_string(), " sito ", "contraseña-1").await;
    assert_eq!((info.user.as_str(), info.name.as_str()), ("Sito", "mock"));
    ready(&mut h, 1).await;
    let seen = gw.routes().seen();
    let post = seen.iter().find(|r| r.method == "POST").unwrap();
    assert_eq!(post.path, "/auth/login");
    assert_eq!(post.authorization, None);
    let body = String::from_utf8_lossy(&post.body);
    assert!(
        body.contains(r#""username":"sito","password":"contraseña-1""#),
        "{body}"
    );
    let events = seen.iter().find(|r| r.path == "/events").unwrap();
    assert_eq!(events.authorization.as_deref(), Some("Bearer key-1"));
    assert_eq!(events.accept.as_deref(), Some("text/event-stream"));
    assert_eq!(hub.subscribers(), 1);

    // The stream dies (silent): reconnects by itself, resuming, and keeps the token.
    hub.go_silent();
    within(
        Duration::from_secs(60),
        h.wait_link(|v| v.state == LinkState::Ready { session: 2 }),
    )
    .await;
    assert_eq!(secrets.account_deletes.load(Ordering::SeqCst), 0);
    // The read timeout outlasted the IPC idle close: the app reconnects.
    let mut app = ipc.client().await;
    let resumed = gw
        .routes()
        .seen()
        .into_iter()
        .rfind(|r| r.path == "/events");
    assert!(resumed.unwrap().last_event_id.is_some());

    // The device is removed (web or admin): 401, the account is deleted once, no retry.
    gw.routes()
        .set("POST", "/auth/login", 201, login_ok("key-2"));
    hub.revoke();
    within(
        Duration::from_secs(10),
        h.wait_link(|v| v.state == LinkState::NotPaired),
    )
    .await;
    assert_eq!(phase(&mut app).await, LinkPhase::NotPaired);
    let opens = hub.stream_opens();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(hub.stream_opens(), opens, "no reconnect after revocation");
    assert_eq!(secrets.account_deletes.load(Ordering::SeqCst), 1);
    assert!(!secrets.inner.contains(SecretKey::GatewayAccount));

    // Signing in again works and the link comes up with the new token, on a fresh stream.
    hub.restore();
    login(&mut app, &gw.addr().to_string(), "sito", "contraseña-1").await;
    ready(&mut h, 3).await;
    let fresh = gw
        .routes()
        .seen()
        .into_iter()
        .rfind(|r| r.path == "/events");
    let fresh = fresh.unwrap();
    assert_eq!(
        (fresh.authorization.as_deref(), fresh.last_event_id),
        (Some("Bearer key-2"), None)
    );

    // Logout: the gateway hears it with the token, the account is gone, no reconnect.
    assert!(logout(&mut app).await);
    let out = gw
        .routes()
        .seen()
        .into_iter()
        .rfind(|r| r.path == "/auth/logout")
        .unwrap();
    assert_eq!(out.authorization.as_deref(), Some("Bearer key-2"));
    assert_eq!(secrets.account_deletes.load(Ordering::SeqCst), 2);
    within(
        Duration::from_secs(10),
        h.wait_link(|v| v.state == LinkState::NotPaired),
    )
    .await;
    let opens = hub.stream_opens();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(hub.stream_opens(), opens, "no reconnect after logout");
    // (The mock notices a closed socket only at its next write: no subscriber check here.)

    drop(app);
    assert!(agent.shutdown(Duration::from_secs(5)).await.clean);
    // The mock's per-stream tasks only end at their next write: stop the mock first, then
    // nothing of the agent may be left.
    gw.shutdown().await;
    drop(hub);
    within(Duration::from_secs(5), async {
        while alive_tasks() > baseline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
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

    // S4 PR 4a, the chat over `/events`: with nothing open a text creates a conversation; its
    // `message` and the turn the gateway's fake OpenClaw answers (`ok`: one delta and a final)
    // arrive over the stream; the list shows it; «Nueva», then opening it again shows its
    // history (`GET /conversations/{id}` + `…/messages`).
    let mut chat = App::open(&ipc).await;
    assert_eq!(
        chat.ask(IpcRequest::SendText {
            text: fogar_ports::ipc::UserText("ok".into()),
        })
        .await,
        IpcResponse::TextAccepted
    );
    chat.until("the reply", Duration::from_secs(30), |a| {
        let mine = a.view.entries.iter().any(|e| {
            matches!(e, ViewEntry::Message(m) if m.mine && m.text == "ok" && m.state == MessageState::Delivered)
        });
        let done = a.view.entries.iter().any(|e| {
            matches!(e, ViewEntry::Turn(t) if t.outcome == Some(TurnOutcome::Done) && !t.reply.is_empty())
        });
        mine && done
    })
    .await;
    let id = chat.view.id.clone().expect("the new conversation is open");
    chat.until("listed, not running", Duration::from_secs(30), |a| {
        a.list.iter().any(|c| c.id == id && !c.running)
    })
    .await;
    assert_eq!(
        chat.ask(IpcRequest::NewConversation).await,
        IpcResponse::ConversationOpened
    );
    chat.until("nothing open", Duration::from_secs(10), |a| {
        a.view.id.is_none() && a.view.entries.is_empty()
    })
    .await;
    assert_eq!(
        chat.ask(IpcRequest::OpenConversation { id: id.clone() })
            .await,
        IpcResponse::ConversationOpened
    );
    chat.until("its history", Duration::from_secs(30), |a| {
        a.view.id.as_deref() == Some(id.as_str())
            && a.view.entries.iter().any(|e| {
                matches!(e, ViewEntry::Message(m) if m.text == "ok" && m.state == MessageState::Past)
            })
            && a.view.entries.iter().any(|e| matches!(e, ViewEntry::Turn(t) if !t.reply.is_empty()))
    })
    .await;
    drop(chat);

    // Restart: the `/events` stream ends; the link reconnects by itself once the gateway is
    // back, same token.
    gw.cmd("restart", "restarted", Duration::from_secs(180))
        .await;
    within(
        Duration::from_secs(30),
        h.wait_link(|v| matches!(v.state, LinkState::Ready { session } if session > 1)),
    )
    .await;
    assert_eq!(secrets.account_deletes.load(Ordering::SeqCst), 0);

    // The admin removes the device: the stream is cut, the reopen gets 401, the account is
    // deleted once, no retry.
    gw.cmd("revoke", "revoked", Duration::from_secs(10)).await;
    let v = within(
        Duration::from_secs(10),
        h.wait_link(|v| v.state == LinkState::NotPaired),
    )
    .await;
    assert_eq!(v.last_close.map(|c| c.0), Some(401));
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
