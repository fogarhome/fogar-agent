//! THE slice-0 end-to-end test: fake microphone -> energy VAD -> core turn machine -> REAL
//! WebSocket (fogar-host, tokio-tungstenite) -> protocol-v1 mock gateway (fogar-testkit) ->
//! TTS frames 0xA1 + turn_seq + PCM16 -> fake player. Real time, loopback, a few seconds total.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use fogar_core::{Agent, AgentPorts, Config, HealthProbe, LinkState, NoIpc, crashloop::BootMode};
use fogar_host::{TokioClock, WsConfig, WsGatewayLink};
use fogar_proto::CloseAction;
use fogar_testkit::{
    FakeMic, FakeSpeaker, MemorySecretStore, MockConfig, MockGateway, SeededRandom, gateway::Ctl,
    silence_chunks, tone_chunks,
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
}

fn fast_config() -> Config {
    let mut c = Config::default();
    c.vad.hangover_chunks = 10;
    c.backoff.base = Duration::from_millis(20);
    c.backoff.cap = Duration::from_millis(200);
    c.backoff.floor = Duration::from_millis(5);
    c
}

async fn start(mock: MockConfig, mut cfg: Config, key: &str) -> E2e {
    let gw = MockGateway::start(mock).await.unwrap();
    cfg.gateway = Some(gw.addr());
    let baseline = alive_tasks();
    let mic = FakeMic::healthy();
    let speaker = FakeSpeaker::healthy();
    let ports: AgentPorts<TokioClock, WsGatewayLink, NoIpc> = AgentPorts {
        clock: Arc::new(TokioClock::new()),
        link: WsGatewayLink::new(WsConfig::default()),
        ipc: None,
        secrets: Arc::new(MemorySecretStore::with_client_key(key)),
        random: Arc::new(SeededRandom::new(3)),
        audio_in: Arc::new(mic.clone()),
        audio_out: Arc::new(speaker.clone()),
        probe: None,
    };
    let agent = Agent::start(ports, cfg, BootMode::Normal);
    let health = agent.health();
    E2e {
        baseline,
        gw,
        agent,
        health,
        mic,
        speaker,
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

/// Pushes a scripted utterance at 4x real time (the VAD only counts chunks).
async fn speak(mic: &FakeMic) {
    for c in tone_chunks(12, 0.3).into_iter().chain(silence_chunks(12)) {
        mic.push(c);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mic_to_gateway_to_player_over_a_real_websocket() {
    let mut e = start(
        MockConfig::default(),
        fast_config(),
        &MockConfig::default().client_key,
    )
    .await;
    ready(&mut e.health, 1).await;
    assert!(alive_tasks() > e.baseline);
    speak(&e.mic).await;
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
    assert_eq!(log.last_output_mode, ["audio", "text"]);
    assert_eq!(
        log.client_msgs
            .iter()
            .filter(|m| *m != "keepalive")
            .collect::<Vec<_>>(),
        ["end", "send"]
    );
    assert_eq!(
        log.sends,
        ["pon la serie"],
        "the final transcription is sent back as the turn text"
    );
    assert!(log.audio_frames >= 12);
    assert_eq!(log.audio_bytes % 4, 0, "f32le upload");
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
async fn barge_in_over_websocket() {
    let mock = MockConfig {
        tts_frames: 500,
        frame_interval: Duration::from_millis(10),
        ..MockConfig::default()
    };
    let mut e = start(mock, fast_config(), &MockConfig::default().client_key).await;
    ready(&mut e.health, 1).await;
    speak(&e.mic).await;
    eventually(Duration::from_secs(10), || {
        e.health.snapshot().stats.tts_frames_played >= 5
    })
    .await;
    speak(&e.mic).await; // user talks over the answer
    eventually(Duration::from_secs(10), || {
        e.health.snapshot().stats.turns_started >= 2
    })
    .await;
    let log = e.gw.log();
    assert_eq!(log.count("cancel"), 1);
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
                code: 1008,
                reason: "revoked".into(),
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
            within(
                Duration::from_secs(10),
                e.health
                    .wait_link(|v| v.state == LinkState::Parked { reason: action }),
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
async fn wrong_key_is_rejected_at_handshake_and_parks() {
    let mut e = start(MockConfig::default(), fast_config(), "not-the-key").await;
    let v = within(
        Duration::from_secs(10),
        e.health
            .wait_link(|v| matches!(v.state, LinkState::Parked { .. })),
    )
    .await;
    assert_eq!(v.last_close, Some((1008, CloseAction::Repair)));
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
    let addr = fogar_ports::gateway::GatewayAddr::parse(&format!("127.0.0.1:{port}")).unwrap();
    let ports: AgentPorts<TokioClock, WsGatewayLink, NoIpc> = AgentPorts {
        clock: Arc::new(TokioClock::new()),
        link: WsGatewayLink::new(WsConfig::default()),
        ipc: None,
        secrets: Arc::new(MemorySecretStore::with_client_key(
            &MockConfig::default().client_key,
        )),
        random: Arc::new(SeededRandom::new(5)),
        audio_in: Arc::new(FakeMic::minimal()),
        audio_out: Arc::new(FakeSpeaker::minimal()),
        probe: None,
    };
    let cfg = Config {
        gateway: Some(addr),
        ..fast_config()
    };
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
