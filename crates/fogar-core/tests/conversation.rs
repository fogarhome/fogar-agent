//! S2 sub-PR 3a acceptance (paused time, fakes + mock gateway + in-memory IPC): text and the
//! conversation view, no microphone. The microphone is never opened in any of these cases.
#![allow(clippy::disallowed_methods)] // the harness drives tokio's paused clock directly
#![allow(clippy::unwrap_used, clippy::field_reassign_with_default)]

mod common;
use common::*;
use fogar_ports::audio::MicPermission;
use fogar_ports::ipc::{
    CaptureGesture, ConversationView, IpcRequest, IpcResponse, ProgressKind, SEND_TEXT_MAX_BYTES,
    ServiceState, TextRefusal, TurnAudio, TurnOutcome, TurnSource, UserActivity, UserText,
};
use fogar_proto::{OutputMode, server::ProgressPhase};
use fogar_testkit::{FakeIpcClient, MemorySecretStore, MockConfig};
use std::{sync::Arc, time::Duration};

fn text(t: &str) -> IpcRequest {
    IpcRequest::SendText {
        text: UserText(t.to_owned()),
    }
}

/// A connection subscribed to the conversation (its first view returned).
async fn subscribe(rig: &Rig) -> (FakeIpcClient, ConversationView) {
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
    let mut rig = rig_with(paced(50)).start();
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
    let mut rig = rig_with(paced(400)).start();
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
    for req in [
        IpcRequest::CaptureStart {
            gesture: CaptureGesture::Toggle,
        },
        IpcRequest::RequestMicPermission,
        IpcRequest::TurnVisible {
            turn_id: "t-1".into(),
        },
    ] {
        assert_eq!(
            c.request(req).await,
            Some(IpcResponse::Refused {
                reason: "unsupported request"
            })
        );
    }
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
