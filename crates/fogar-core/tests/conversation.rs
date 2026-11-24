//! S2 sub-PR 3a acceptance (paused time, fakes + mock gateway + in-memory IPC): text and the
//! conversation view, no microphone. The microphone is never opened in any of these cases.
#![allow(clippy::disallowed_methods)] // the harness drives tokio's paused clock directly
#![allow(clippy::unwrap_used, clippy::field_reassign_with_default)]

mod common;
use common::*;
use fogar_ports::ipc::{
    CaptureGesture, CaptureRefusal, CaptureThen, ConversationView, IpcRequest, IpcResponse,
    ProgressKind, SEND_TEXT_MAX_BYTES, ServiceState, TextRefusal, TurnAudio, TurnOutcome,
    TurnSource, UserActivity, UserText,
};
use fogar_ports::{audio::MicPermission, gateway::GatewayLink};
use fogar_proto::{OutputMode, server::ProgressPhase};
use fogar_testkit::{FakeIpcClient, MemorySecretStore, MockConfig, gateway::Ctl};
use std::{sync::Arc, time::Duration};

fn text(t: &str) -> IpcRequest {
    IpcRequest::SendText {
        text: UserText(t.to_owned()),
    }
}

/// A connection subscribed to the conversation (its first view returned).
async fn subscribe<L: GatewayLink>(rig: &Rig<L>) -> (FakeIpcClient, ConversationView) {
    let mut c = rig.ipc.as_ref().unwrap().client().await;
    assert_eq!(
        c.request(IpcRequest::SubscribeConversation).await,
        Some(IpcResponse::ConversationSubscribed)
    );
    match c.next().await {
        Some(IpcResponse::Conversation(v)) => (c, v),
        other => panic!("{other:?}"),
    }
}

/// Pushed views until one satisfies `pred` (latest-only pushes may skip intermediate ones).
async fn until(
    sub: &mut FakeIpcClient,
    pred: impl Fn(&ConversationView) -> bool,
) -> ConversationView {
    within(Duration::from_secs(60), async {
        loop {
            match sub.next().await {
                Some(IpcResponse::Conversation(v)) if pred(&v) => return v,
                Some(IpcResponse::Conversation(_)) => {}
                other => panic!("{other:?}"),
            }
        }
    })
    .await
}

fn rig_with(mock: MockConfig) -> RigBuilder<fogar_testkit::FakeGatewayLink> {
    let mut b = RigBuilder::fake(mock);
    b.with_ipc = true;
    b
}

fn paced(frames: u32) -> MockConfig {
    MockConfig {
        tts_frames: frames,
        frame_interval: Duration::from_millis(20),
        ..MockConfig::default()
    }
}

#[tokio::test(start_paused = true)]
async fn typed_text_shows_as_a_turn_with_reply_then_done() {
    // The app shows the conversation: it confirms the turn, so its audio plays.
    let mut rig = rig_with(paced(50)).app_on_screen().start();
    wait_ready(&mut rig.health).await;
    let (mut sub, first) = subscribe(&rig).await;
    assert!(first.turns.is_empty());
    assert_eq!(
        first.mic,
        MicPermission::Granted,
        "permission status reaches the view"
    );
    assert_eq!(
        (first.transcriber, first.tts),
        (ServiceState::Up, ServiceState::Up)
    );
    let mut ui = rig.ipc.as_ref().unwrap().client().await;
    assert_eq!(
        ui.request(text("pon la serie")).await,
        Some(IpcResponse::TextAccepted)
    );

    // The reply streams while the audio plays (turn still open)...
    let v = until(&mut sub, |v| {
        v.turns
            .last()
            .is_some_and(|t| t.reply == "Vale, pongo la serie.")
    })
    .await;
    let t = v.turns.last().unwrap();
    assert_eq!(
        (t.turn_id.as_str(), t.source, t.mine.as_str()),
        ("t-1", TurnSource::Mine, "pon la serie")
    );
    assert_eq!(t.progress, None, "tokens make the progress stale");
    // ... then it ends.
    let v = until(&mut sub, |v| {
        v.turns.last().is_some_and(|t| t.outcome.is_some())
    })
    .await;
    let t = v.turns.last().unwrap();
    assert_eq!(t.outcome, Some(TurnOutcome::Done));
    assert_eq!(t.audio, TurnAudio::Played);
    assert_eq!(v.activity, UserActivity::Idle);
    assert_eq!(v.turns.len(), 1);
    assert_eq!(rig.link.log().sends, ["pon la serie"]);
    // The status report carries the voice state too.
    match ui.request(IpcRequest::Status).await {
        Some(IpcResponse::Status(s)) => {
            assert_eq!((s.transcriber, s.tts), (ServiceState::Up, ServiceState::Up))
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(rig.mic.opens(), 0);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn progress_and_tool_show_while_no_text_arrives() {
    let mut b = rig_with(MockConfig {
        progress: vec![(ProgressPhase::Tool, Some("ha_call_service".into()))],
        ..paced(50)
    });
    // No `text`: nothing makes the progress stale before turn_end.
    b.cfg.output_mode = vec![OutputMode::Audio, OutputMode::Status];
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let (mut sub, _) = subscribe(&rig).await;
    let mut ui = rig.ipc.as_ref().unwrap().client().await;
    assert_eq!(
        ui.request(text("enciende la luz")).await,
        Some(IpcResponse::TextAccepted)
    );
    let v = until(&mut sub, |v| {
        v.turns.last().is_some_and(|t| t.progress.is_some())
    })
    .await;
    let t = v.turns.last().unwrap();
    assert_eq!(t.progress, Some(ProgressKind::Tool));
    assert_eq!(t.tool.as_deref(), Some("ha_call_service"));
    let v = until(&mut sub, |v| {
        v.turns.last().is_some_and(|t| t.outcome.is_some())
    })
    .await;
    let t = v.turns.last().unwrap();
    assert_eq!(
        (t.outcome, t.progress, t.tool.as_deref()),
        (Some(TurnOutcome::Done), None, None)
    );
    assert_eq!(rig.mic.opens(), 0);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn cancel_mid_turn_sends_cancel_and_ends_cancelled() {
    let mut rig = rig_with(paced(400)).app_on_screen().start();
    wait_ready(&mut rig.health).await;
    let (mut sub, _) = subscribe(&rig).await;
    let mut ui = rig.ipc.as_ref().unwrap().client().await;
    assert_eq!(
        ui.request(text("cuéntame un cuento")).await,
        Some(IpcResponse::TextAccepted)
    );
    until(&mut sub, |v| {
        v.turns.last().is_some_and(|t| t.audio == TurnAudio::Played)
    })
    .await;
    assert_eq!(
        ui.request(IpcRequest::CancelTurn).await,
        Some(IpcResponse::TurnCancelSent)
    );
    let v = until(&mut sub, |v| {
        v.turns.last().is_some_and(|t| t.outcome.is_some())
    })
    .await;
    assert_eq!(
        v.turns.last().unwrap().outcome,
        Some(TurnOutcome::Cancelled)
    );
    let log = rig.link.log();
    assert_eq!(log.count("cancel"), 1);
    assert!(log.tts_frames_sent < 400, "the gateway stopped streaming");
    assert!(
        rig.speaker.snapshot().stops >= 1,
        "playback stopped at once"
    );
    // Cancelling with nothing open sends nothing more.
    assert_eq!(
        ui.request(IpcRequest::CancelTurn).await,
        Some(IpcResponse::TurnCancelSent)
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(rig.link.log().count("cancel"), 1);
    assert_eq!(rig.mic.opens(), 0);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn uid_only_peers_are_refused_even_with_the_pairing_opt_in() {
    let mut b = rig_with(MockConfig::default());
    b.signed_peer = false;
    // The development opt-in for Pair/Unpair must NOT extend to the conversation.
    b.cfg.pair_from_uid_only_peers = true;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let mut c = rig.ipc.as_ref().unwrap().client().await;
    for req in [
        IpcRequest::SubscribeConversation,
        text("hola"),
        IpcRequest::CancelTurn,
        IpcRequest::CaptureStart {
            gesture: CaptureGesture::Hold,
        },
        IpcRequest::CaptureEnd {
            then: CaptureThen::Send,
        },
        IpcRequest::TurnVisible {
            turn_id: "t-1".into(),
        },
        IpcRequest::RequestMicPermission,
    ] {
        assert_eq!(
            c.request(req).await,
            Some(IpcResponse::Refused {
                reason: "needs the signed Fogar app"
            })
        );
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(rig.link.log().sends.is_empty());
    assert_eq!(rig.mic.opens(), 0);
    assert_eq!(rig.mic.permission_requests(), 0);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn gateway_without_voice_still_does_text() {
    let mut rig = rig_with(MockConfig {
        voice: false,
        ..MockConfig::default()
    })
    .start();
    wait_ready(&mut rig.health).await;
    let (mut sub, first) = subscribe(&rig).await;
    assert_ne!(first.transcriber, ServiceState::Up);
    assert_ne!(first.tts, ServiceState::Up);
    let mut ui = rig.ipc.as_ref().unwrap().client().await;
    assert_eq!(
        ui.request(text("hola")).await,
        Some(IpcResponse::TextAccepted)
    );
    let v = until(&mut sub, |v| {
        v.turns.last().is_some_and(|t| t.outcome.is_some())
    })
    .await;
    let t = v.turns.last().unwrap();
    assert_eq!(
        (t.reply.as_str(), t.outcome),
        ("Vale, pongo la serie.", Some(TurnOutcome::Done))
    );
    assert_eq!(t.audio, TurnAudio::None);
    match ui.request(IpcRequest::Status).await {
        Some(IpcResponse::Status(s)) => {
            assert!(s.transcriber != ServiceState::Up && s.tts != ServiceState::Up)
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(rig.mic.opens(), 0);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn text_refusals_and_later_requests_never_touch_the_mic() {
    let mut rig = rig_with(MockConfig::default()).start();
    wait_ready(&mut rig.health).await;
    let mut c = rig.ipc.as_ref().unwrap().client().await;
    assert_eq!(
        c.request(text("  \n ")).await,
        Some(IpcResponse::TextRefused(TextRefusal::Empty))
    );
    let long = "a".repeat(SEND_TEXT_MAX_BYTES + 1);
    assert_eq!(
        c.request(text(&long)).await,
        Some(IpcResponse::TextRefused(TextRefusal::TooLong))
    );
    // 3b/3c: a connection that does not show the conversation cannot open a take, nor
    // confirm a turn as visible.
    assert_eq!(
        c.request(IpcRequest::CaptureStart {
            gesture: CaptureGesture::Toggle,
        })
        .await,
        Some(IpcResponse::CaptureRefused(CaptureRefusal::NotSubscribed))
    );
    assert_eq!(
        c.request(IpcRequest::TurnVisible {
            turn_id: "t-1".into(),
        })
        .await,
        Some(IpcResponse::Refused {
            reason: "not subscribed to the conversation"
        })
    );
    assert!(rig.link.log().sends.is_empty());
    assert_eq!((rig.mic.opens(), rig.mic.permission_requests()), (0, 0));
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn text_is_refused_while_the_link_is_not_ready() {
    let mut b = rig_with(MockConfig::default());
    b.secrets = Arc::new(MemorySecretStore::default()); // not paired
    let mut rig = b.start();
    wait_state(&mut rig.health, |v| {
        v.state == fogar_core::LinkState::NotPaired
    })
    .await;
    let (mut sub, first) = subscribe(&rig).await;
    assert_eq!(
        (first.transcriber, first.tts),
        (ServiceState::Unknown, ServiceState::Unknown)
    );
    let mut c = rig.ipc.as_ref().unwrap().client().await;
    assert_eq!(
        c.request(text("hola")).await,
        Some(IpcResponse::TextRefused(TextRefusal::NotReady))
    );
    assert_eq!(
        c.request(IpcRequest::CancelTurn).await,
        Some(IpcResponse::TurnCancelSent)
    );
    // A subscribed connection is not closed for being idle.
    tokio::time::sleep(rig_idle() * 3).await;
    drop(c);
    let mut again = rig.ipc.as_ref().unwrap().client().await;
    assert_eq!(
        again.request(text("x")).await,
        Some(IpcResponse::TextRefused(TextRefusal::NotReady))
    );
    // The subscriber outlived 3 idle periods and still answers.
    let mut r = sub.request(IpcRequest::Status).await;
    while matches!(r, Some(IpcResponse::Conversation(_))) {
        r = sub.next().await;
    }
    assert!(matches!(r, Some(IpcResponse::Status(_))), "{r:?}");
    assert_eq!(rig.mic.opens(), 0);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

fn rig_idle() -> Duration {
    test_config().ipc_idle_timeout
}

/// Extra server frames injected into the mock session (a dense token stream).
mod flood {
    use fogar_ports::gateway::{
        Frame, GatewayAddr, GatewayLink, LinkError, LinkEvent, LinkSession,
    };
    use fogar_testkit::FakeGatewayLink;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    #[derive(Clone)]
    pub(super) struct FloodLink {
        pub(super) inner: FakeGatewayLink,
        pub(super) rx: Arc<Mutex<Option<mpsc::Receiver<String>>>>,
    }
    pub(super) struct FloodSess {
        inner: <FakeGatewayLink as GatewayLink>::Session,
        rx: Option<mpsc::Receiver<String>>,
    }
    impl LinkSession for FloodSess {
        fn max_frame(&self) -> usize {
            self.inner.max_frame()
        }
        async fn send(&mut self, f: Frame) -> Result<(), LinkError> {
            self.inner.send(f).await
        }
        async fn recv(&mut self) -> Option<LinkEvent> {
            match self.rx.as_mut() {
                Some(rx) => tokio::select! {
                    biased;
                    Some(t) = rx.recv() => Some(LinkEvent::Frame(Frame::Text(t))),
                    ev = self.inner.recv() => ev,
                },
                None => self.inner.recv().await,
            }
        }
        async fn close(&mut self, c: u16, r: &str) -> Result<(), LinkError> {
            self.inner.close(c, r).await
        }
    }
    impl GatewayLink for FloodLink {
        type Session = FloodSess;
        async fn connect(
            &self,
            addr: &GatewayAddr,
            route: fogar_ports::gateway::WsRoute,
        ) -> Result<FloodSess, LinkError> {
            let inner = self.inner.connect(addr, route).await?;
            let rx = self.rx.lock().ok().and_then(|mut g| g.take());
            Ok(FloodSess { inner, rx })
        }
    }
}

/// Review M2: pushes must not starve requests. The SAME subscribed connection gets a dense
/// token stream and still has its `CancelTurn` answered within one `write_timeout`.
#[tokio::test(start_paused = true)]
async fn requests_are_read_under_a_dense_push_stream() {
    let mock = paced(100_000);
    let inner = fogar_testkit::FakeGatewayLink::new(mock.clone());
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(20_000);
    let link = flood::FloodLink {
        inner: inner.clone(),
        rx: Arc::new(std::sync::Mutex::new(Some(rx))),
    };
    let mut b = RigBuilder::with_link(link, &mock.client_key);
    b.with_ipc = true;
    let write_timeout = b.cfg.write_timeout;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let (mut sub, _) = subscribe(&rig).await;
    assert_eq!(inner.control(Ctl::Push), 1);
    until(&mut sub, |v| {
        v.turns.last().is_some_and(|t| t.turn_id == "t-1")
    })
    .await;
    // Frames already waiting in the transport: the stream never pauses.
    for _ in 0..20_000 {
        let tok = r#"{"type":"token","turn_id":"t-1","text":"x"}"#.to_owned();
        tx.try_send(tok).unwrap();
    }
    // Let the stream get going, then press cancel on the subscribed connection.
    let mut pushes = 0;
    while pushes < 50 {
        if let Some(IpcResponse::Conversation(_)) = sub.next().await {
            pushes += 1;
        }
    }
    let t0 = tokio::time::Instant::now();
    let mut r = sub.request(IpcRequest::CancelTurn).await;
    let mut waited_pushes = 0u32;
    while matches!(r, Some(IpcResponse::Conversation(_))) {
        waited_pushes += 1;
        assert!(waited_pushes < 100_000, "CancelTurn never read");
        r = sub.next().await;
    }
    assert_eq!(r, Some(IpcResponse::TurnCancelSent));
    assert!(
        t0.elapsed() <= write_timeout,
        "answered after {:?}",
        t0.elapsed()
    );
    drop(tx);
    assert_eq!(rig.mic.opens(), 0);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

/// A gateway `TURN_ERROR` (here the 5.0 rate limit) reaches the app's view as `error_code`.
#[tokio::test(start_paused = true)]
async fn orchestrator_rate_limited_reaches_the_view_as_error_code() {
    let mock = paced(100_000);
    let inner = fogar_testkit::FakeGatewayLink::new(mock.clone());
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(4);
    let link = flood::FloodLink {
        inner: inner.clone(),
        rx: Arc::new(std::sync::Mutex::new(Some(rx))),
    };
    let mut b = RigBuilder::with_link(link, &mock.client_key);
    b.with_ipc = true;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let (mut sub, _) = subscribe(&rig).await;
    assert_eq!(inner.control(Ctl::Push), 1);
    until(&mut sub, |v| {
        v.turns.last().is_some_and(|t| t.turn_id == "t-1")
    })
    .await;
    tx.try_send(
        r#"{"type":"error","code":"TURN_ERROR","message":"orchestrator_rate_limited","fatal":false,"turn_id":"t-1"}"#
            .to_owned(),
    )
    .unwrap();
    let v = until(&mut sub, |v| {
        v.turns.last().is_some_and(|t| t.outcome.is_some())
    })
    .await;
    let t = v.turns.last().unwrap();
    assert_eq!(t.outcome, Some(TurnOutcome::Error));
    assert_eq!(t.error_code.as_deref(), Some("orchestrator_rate_limited"));
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

/// A uid-only status subscriber never sees the conversation, even while a turn streams.
#[tokio::test(start_paused = true)]
async fn uid_only_status_subscriber_never_gets_the_conversation() {
    let mut b = rig_with(paced(50));
    b.signed_peer = false;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let mut c = rig.ipc.as_ref().unwrap().client().await;
    assert_eq!(
        c.request(IpcRequest::Subscribe).await,
        Some(IpcResponse::Subscribed)
    );
    assert!(matches!(c.next().await, Some(IpcResponse::Event(_))));
    assert_eq!(rig.link.control(Ctl::Push), 1);
    // Nobody can confirm the push as visible (no signed app): it never sounds.
    eventually(Duration::from_secs(30), || {
        rig.health.snapshot().stats.tts_samples_muted == 50 * 480
    })
    .await;
    assert_eq!(rig.speaker.snapshot().opens, 0);
    let drain = async {
        while let Some(m) = c.next().await {
            assert!(matches!(m, IpcResponse::Event(_)), "{m:?}");
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), drain).await;
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

/// S2 3c (single output point): only a connection that shows the conversation can confirm a
/// turn, and its confirmation is what lets the turn sound. The view shows the wait, then that
/// the turn played.
#[tokio::test(start_paused = true)]
async fn turn_visible_from_the_subscribed_app_lets_the_turn_sound() {
    let mut rig = rig_with(paced(20)).start();
    wait_ready(&mut rig.health).await;
    // A signed connection that does not show the conversation cannot confirm anything.
    let mut other = rig.ipc.as_ref().unwrap().client().await;
    let r = other
        .request(IpcRequest::TurnVisible {
            turn_id: "x".into(),
        })
        .await;
    assert!(matches!(r, Some(IpcResponse::Refused { .. })), "{r:?}");
    let (mut sub, _) = subscribe(&rig).await;
    assert_eq!(rig.link.control(Ctl::Push), 1);
    let v = until(&mut sub, |v| v.awaiting_visible.is_some()).await;
    let id = v.awaiting_visible.unwrap();
    assert_eq!(
        rig.speaker.snapshot().opens,
        0,
        "nothing opened before the confirmation"
    );
    let confirm = IpcRequest::TurnVisible {
        turn_id: id.clone(),
    };
    sub.send_half(&confirm, true).await;
    sub.send_half(&confirm, false).await;
    within(Duration::from_secs(10), async {
        loop {
            match sub.next().await {
                Some(IpcResponse::TurnVisibleNoted) => break,
                Some(IpcResponse::Conversation(_)) => {}
                other => panic!("{other:?}"),
            }
        }
    })
    .await;
    let v = until(&mut sub, |v| {
        v.turns
            .iter()
            .any(|t| t.turn_id == id && t.audio == TurnAudio::Played)
    })
    .await;
    assert_eq!(v.awaiting_visible, None);
    eventually(Duration::from_secs(10), || {
        rig.speaker.snapshot().accepted == 20 * 480
    })
    .await;
    assert_eq!(rig.health.snapshot().stats.tts_turns_seen, 1);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}
