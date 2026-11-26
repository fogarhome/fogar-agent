//! THE end-to-end test: typed text over the in-memory IPC -> core turn machine -> REAL
//! WebSocket (fogar-host, tokio-tungstenite) -> protocol-v1 mock gateway (fogar-testkit) ->
//! TTS frames 0xA1 + turn_seq + PCM16 -> fake player. Real time, loopback, a few seconds total.
//! (S2 3a: no microphone at start; the voice path returns with the capture gate in 3b.)
//! (S2 3c: nothing sounds unless the app confirms the turn is on screen; see `app_on_screen`.)
#![allow(clippy::unwrap_used, clippy::expect_used)]

use fogar_core::{
    AccountRecord, Agent, AgentPorts, Config, HealthProbe, LinkState, NoHttp, NoIpc,
    crashloop::BootMode,
};
use fogar_host::{TokioClock, WsConfig, WsGatewayLink};
use fogar_ports::{
    gateway::GatewayAddr,
    ipc::{IpcRequest, IpcResponse, UserText},
    secrets::{SecretKey, SecretStore as _},
};
use fogar_proto::CloseAction;
use fogar_testkit::{
    FakeIpc, FakeMic, FakeSpeaker, MemorySecretStore, MockConfig, MockGateway, SeededRandom,
    gateway::Ctl,
};
use std::{sync::Arc, time::Duration};

struct E2e {
    /// Runtime live tasks with the mock gateway up and the agent not yet started.
    baseline: usize,
    gw: MockGateway,
    agent: Agent<TokioClock>,
    health: HealthProbe,
    mic: FakeMic,
    speaker: FakeSpeaker,
    ipc: FakeIpc,
    /// The app with the conversation on screen (ends when the agent closes its connection).
    _app: tokio::task::JoinSet<()>,
}

/// A TEST MODEL of the app with its conversation window on screen, NOT the app's behaviour
/// (the real app confirms its own turns at `turn_start` and a push only while the window is
/// visible). It subscribes to the conversation, reports when the subscription is answered, and
/// confirms with `TurnVisible` each entry the agent says waits for it (`awaiting_visible`).
/// - The view carries no `turn_seq`, so an entry is keyed by its id plus how many entries carry
///   that id: a new turn reusing the previous id is a new key and is not skipped.
/// - "engine busy" is retried, not taken as a confirmation; any other refusal is final.
async fn app_on_screen(ipc: FakeIpc, subscribed: tokio::sync::oneshot::Sender<()>) {
    async fn send(c: &mut fogar_testkit::FakeIpcClient, r: IpcRequest) {
        c.send_half(&r, true).await;
        c.send_half(&r, false).await;
    }
    let mut c = ipc.client().await;
    send(&mut c, IpcRequest::SubscribeConversation).await;
    if c.next().await != Some(IpcResponse::ConversationSubscribed) {
        return;
    }
    let _ = subscribed.send(());
    let mut latest: Option<(String, usize)> = None;
    let mut asked: Option<(String, usize)> = None;
    let mut done: Option<(String, usize)> = None;
    while let Some(m) = c.next().await {
        match m {
            IpcResponse::Conversation(v) => {
                latest = v.awaiting_visible.map(|id| {
                    let n = v.turns.iter().filter(|t| t.turn_id == id).count();
                    (id, n)
                });
            }
            IpcResponse::TurnVisibleNoted => done = asked.take(),
            IpcResponse::Refused {
                reason: "engine busy",
            } => {
                asked = None;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            IpcResponse::Refused { .. } => done = asked.take(),
            _ => {}
        }
        if asked.is_none()
            && let Some(k) = latest.clone()
            && done.as_ref() != Some(&k)
        {
            send(
                &mut c,
                IpcRequest::TurnVisible {
                    turn_id: k.0.clone(),
                },
            )
            .await;
            asked = Some(k);
        }
    }
}

/// A store signed in at the gateway at `addr` with token `key`.
fn paired(addr: GatewayAddr, key: &str) -> MemorySecretStore {
    let s = MemorySecretStore::default();
    let rec = AccountRecord::new(addr, "c-1".into(), key.to_owned().into());
    s.put(SecretKey::GatewayAccount, rec.encode()).unwrap();
    s
}

fn fast_config() -> Config {
    let mut c = Config::default();
    c.backoff.base = Duration::from_millis(20);
    c.backoff.cap = Duration::from_millis(200);
    c.backoff.floor = Duration::from_millis(5);
    c
}

async fn start(mock: MockConfig, cfg: Config, key: &str) -> E2e {
    let gw = MockGateway::start(mock).await.unwrap();
    let baseline = alive_tasks();
    let mic = FakeMic::healthy();
    let speaker = FakeSpeaker::healthy();
    let ipc = FakeIpc::signed();
    let ports: AgentPorts<TokioClock, WsGatewayLink, FakeIpc> = AgentPorts {
        clock: Arc::new(TokioClock::new()),
        link: WsGatewayLink::new(WsConfig::default()),
        ipc: Some(ipc.clone()),
        http: NoHttp,
        tailnet: fogar_core::NoTailnet,
        secrets: Arc::new(paired(gw.addr(), key)),
        random: Arc::new(SeededRandom::new(3)),
        audio_in: Arc::new(mic.clone()),
        audio_out: Arc::new(speaker.clone()),
        probe: None,
    };
    let agent = Agent::start(ports, cfg, BootMode::Normal);
    let health = agent.health();
    // The app is subscribed before any test says anything.
    let mut app = tokio::task::JoinSet::new();
    let (subscribed_tx, subscribed) = tokio::sync::oneshot::channel();
    app.spawn(app_on_screen(ipc.clone(), subscribed_tx));
    within(Duration::from_secs(10), subscribed)
        .await
        .expect("the app subscribed to the conversation");
    E2e {
        baseline,
        gw,
        agent,
        health,
        mic,
        speaker,
        ipc,
        _app: app,
    }
}

async fn within<F: std::future::Future>(d: Duration, f: F) -> F::Output {
    tokio::time::timeout(d, f).await.expect("timed out")
}

async fn eventually(d: Duration, mut cond: impl FnMut() -> bool) {
    within(d, async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
}

async fn ready(h: &mut HealthProbe, session: u64) {
    within(
        Duration::from_secs(10),
        h.wait_link(|v| v.state == LinkState::Ready { session }),
    )
    .await;
}

/// The person types `text` in the signed app (one IPC connection per request).
async fn say(ipc: &FakeIpc, text: &str) {
    let mut c = ipc.client().await;
    let r = c
        .request(IpcRequest::SendText {
            text: UserText(text.to_owned()),
        })
        .await;
    assert_eq!(r, Some(IpcResponse::TextAccepted));
}

fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_to_gateway_to_player_over_a_real_websocket() {
    let mut e = start(
        MockConfig::default(),
        fast_config(),
        &MockConfig::default().client_key,
    )
    .await;
    ready(&mut e.health, 1).await;
    assert!(alive_tasks() > e.baseline);
    say(&e.ipc, "pon la serie").await;
    eventually(Duration::from_secs(10), || {
        e.health.snapshot().stats.turns_completed == 1
    })
    .await;

    let log = e.gw.log();
    assert_eq!(log.handshakes_ok, 1);
    assert!(
        !log.handshake_had_type_field,
        "the handshake is the only client JSON without `type`"
    );
    assert_eq!(log.last_output_mode, ["audio", "text", "status"]);
    assert_eq!(
        log.client_msgs
            .iter()
            .filter(|m| *m != "keepalive")
            .collect::<Vec<_>>(),
        ["send"]
    );
    assert_eq!(
        log.sends,
        ["pon la serie"],
        "the typed text is the turn text"
    );
    assert_eq!(log.audio_frames, 0, "no microphone, no upload");
    assert_eq!(e.mic.opens(), 0);
    assert_eq!(log.tts_frames_sent, 5);

    let played = e.speaker.played_samples();
    assert_eq!(played.len(), 5 * 480);
    let expected: Vec<i16> = (0..480).map(|i| ((i * 37) % 2000 - 1000) as i16).collect();
    assert_eq!(&played[..480], &expected[..], "PCM16 arrives bit-exact");
    let s = e.health.snapshot().stats;
    assert_eq!(
        (
            s.turns_started,
            s.tts_frames_played,
            s.tts_frames_stale,
            s.tts_frames_invalid
        ),
        (1, 5, 0, 0)
    );

    let report = e.agent.shutdown(Duration::from_secs(5)).await;
    assert!(report.clean, "{report:?}");
    eventually(Duration::from_secs(5), || e.gw.log().live == 0).await;
    assert_eq!(
        e.gw.log().client_closes,
        1,
        "graceful close frame sent on shutdown"
    );
    // No leaked task: back to exactly the pre-agent count (mock accept loop only).
    eventually(Duration::from_secs(5), || alive_tasks() == e.baseline).await;
    e.gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_text_replaces_the_turn_over_websocket() {
    let mock = MockConfig {
        tts_frames: 500,
        frame_interval: Duration::from_millis(10),
        ..MockConfig::default()
    };
    let mut e = start(mock, fast_config(), &MockConfig::default().client_key).await;
    ready(&mut e.health, 1).await;
    say(&e.ipc, "pon la serie").await;
    eventually(Duration::from_secs(10), || {
        e.health.snapshot().stats.tts_frames_played >= 5
    })
    .await;
    say(&e.ipc, "mejor la otra").await; // typed over the answer
    eventually(Duration::from_secs(10), || {
        e.health.snapshot().stats.turns_started >= 2
    })
    .await;
    let log = e.gw.log();
    assert_eq!(log.count("cancel"), 0, "`send` replaces the turn by itself");
    assert!(log.turns_cancelled >= 1);
    assert_eq!(e.health.snapshot().stats.barge_ins, 1);
    assert!(e.speaker.snapshot().stops >= 1);
    assert!(e.agent.shutdown(Duration::from_secs(5)).await.clean);
    e.gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_closes_are_classified_over_websocket() {
    // (scripted close, classification, reconnects?)
    let cases: [(Ctl, CloseAction, bool); 6] = [
        (
            Ctl::Close {
                code: 1012,
                reason: String::new(),
            },
            CloseAction::ServerRestart,
            true,
        ),
        (Ctl::Abort, CloseAction::RetryBackoff, true), // no close frame -> 1006
        (
            Ctl::Close {
                code: 1013,
                reason: "client too slow".into(),
            },
            CloseAction::RetryBackoff,
            true,
        ),
        (
            Ctl::Close {
                code: 1011,
                reason: "Servicio crítico no disponible.".into(),
            },
            CloseAction::RetryBackoff,
            true,
        ),
        (
            Ctl::Close {
                code: 4200,
                reason: "credential revoked".into(),
            },
            CloseAction::Repair,
            false,
        ),
        (
            Ctl::Close {
                code: 1008,
                reason: "Agent 'x' not permitted for this client.".into(),
            },
            CloseAction::AgentRefused,
            false,
        ),
    ];
    for (cmd, action, reconnects) in cases {
        let mut e = start(
            MockConfig::default(),
            fast_config(),
            &MockConfig::default().client_key,
        )
        .await;
        ready(&mut e.health, 1).await;
        assert_eq!(e.gw.control(cmd.clone()), 1);
        let v = within(
            Duration::from_secs(10),
            e.health.wait_link(|v| v.last_close.is_some()),
        )
        .await;
        assert_eq!(v.last_close.map(|c| c.1), Some(action), "{cmd:?}");
        if reconnects {
            ready(&mut e.health, 2).await;
        } else {
            // Revocation deletes the account (spec §0); other refusals park and keep it.
            let expect = if action == CloseAction::Repair {
                LinkState::NotPaired
            } else {
                LinkState::Parked { reason: action }
            };
            within(
                Duration::from_secs(10),
                e.health.wait_link(|v| v.state == expect),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert_eq!(e.gw.log().connections, 1, "{cmd:?}: parked, no retry");
        }
        assert!(
            e.agent.shutdown(Duration::from_secs(5)).await.clean,
            "{cmd:?}"
        );
        e.gw.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_key_is_rejected_at_handshake_and_unpairs() {
    let mut e = start(MockConfig::default(), fast_config(), "not-the-key").await;
    let v = within(
        Duration::from_secs(10),
        e.health.wait_link(|v| v.state == LinkState::NotPaired),
    )
    .await;
    assert_eq!(v.last_close, Some((4010, CloseAction::Repair)));
    assert_eq!(e.gw.log().handshakes_rejected, 1);
    assert!(e.agent.shutdown(Duration::from_secs(5)).await.clean);
    e.gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keepalive_and_idle_close_over_websocket() {
    // Scaled-down timers: gateway idle 300 ms, client keepalive after 100 ms of silence.
    let mock = MockConfig {
        idle_timeout: Some(Duration::from_millis(300)),
        ..MockConfig::default()
    };
    let mut cfg = fast_config();
    cfg.keepalive_after = Some(Duration::from_millis(100));
    let mut e = start(mock.clone(), cfg, &mock.client_key).await;
    ready(&mut e.health, 1).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let s = e.health.snapshot().stats;
    assert_eq!(s.sessions_started, 1, "keepalives prevent the idle close");
    assert!(s.keepalives_sent >= 8, "{} keepalives", s.keepalives_sent);
    assert!(e.agent.shutdown(Duration::from_secs(5)).await.clean);
    e.gw.shutdown().await;

    let mut cfg = fast_config();
    cfg.keepalive_after = None;
    let mut e = start(mock.clone(), cfg, &mock.client_key).await;
    ready(&mut e.health, 1).await;
    let v = within(
        Duration::from_secs(5),
        e.health.wait_link(|v| v.last_close.is_some()),
    )
    .await;
    assert_eq!(v.last_close, Some((1000, CloseAction::IdleOrNormal)));
    ready(&mut e.health, 2).await;
    assert!(e.agent.shutdown(Duration::from_secs(5)).await.clean);
    e.gw.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_down_then_up_reconnects() {
    // Nothing listens yet: connects fail and back off; then the gateway appears on that port.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let addr = GatewayAddr::parse(&format!("127.0.0.1:{port}")).unwrap();
    let ports: AgentPorts<TokioClock, WsGatewayLink, NoIpc> = AgentPorts {
        clock: Arc::new(TokioClock::new()),
        link: WsGatewayLink::new(WsConfig::default()),
        ipc: None,
        http: NoHttp,
        tailnet: fogar_core::NoTailnet,
        secrets: Arc::new(paired(addr, &MockConfig::default().client_key)),
        random: Arc::new(SeededRandom::new(5)),
        audio_in: Arc::new(FakeMic::minimal()),
        audio_out: Arc::new(FakeSpeaker::minimal()),
        probe: None,
    };
    let cfg = fast_config();
    let agent = Agent::start(ports, cfg, BootMode::Normal);
    let mut h = agent.health();
    within(
        Duration::from_secs(5),
        h.wait_link(|v| matches!(v.state, LinkState::Backoff { attempt, .. } if attempt >= 3)),
    )
    .await;
    assert!(h.snapshot().stats.connect_failures >= 3);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    let gw = MockGateway::from_listener(listener, MockConfig::default()).unwrap();
    within(
        Duration::from_secs(10),
        h.wait_link(|v| matches!(v.state, LinkState::Ready { .. })),
    )
    .await;
    assert!(agent.shutdown(Duration::from_secs(5)).await.clean);
    gw.shutdown().await;
}
