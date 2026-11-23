//! S2 sub-PR 3b acceptance (paused time, fakes + mock gateway + in-memory IPC): takes and the
//! single consent point. Every take here is opened by an explicit `CaptureStart` from a signed,
//! subscribed connection; every other path must leave `FakeMic::opens() == 0`.
#![allow(clippy::disallowed_methods)] // the harness drives tokio's paused clock directly
#![allow(clippy::unwrap_used, clippy::field_reassign_with_default)]

mod common;
use common::*;
use fogar_ports::audio::MicPermission;
use fogar_ports::ipc::{
    CaptureGesture, CaptureRefusal, CaptureThen, ConversationView, IpcRequest, IpcResponse, Notice,
    TextRefusal, UserActivity, UserText,
};
use fogar_testkit::{
    FakeGatewayLink, FakeIpcClient, FakeMic, MockConfig, gateway::Ctl, tone_chunks,
};
use std::time::Duration;

const START: IpcRequest = IpcRequest::CaptureStart {
    gesture: CaptureGesture::Toggle,
};

fn end(then: CaptureThen) -> IpcRequest {
    IpcRequest::CaptureEnd { then }
}

fn rig_with(mock: MockConfig) -> RigBuilder<FakeGatewayLink> {
    let mut b = RigBuilder::fake(mock);
    b.with_ipc = true;
    b
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

/// Sends `req` on a subscribed connection and returns its answer, skipping the pushes that
/// arrive before it.
async fn ask(c: &mut FakeIpcClient, req: IpcRequest) -> IpcResponse {
    within(Duration::from_secs(60), async {
        c.tx_request(req).await;
        loop {
            match c.next().await {
                Some(IpcResponse::Conversation(_)) => {}
                Some(r) => return r,
                None => panic!("connection closed"),
            }
        }
    })
    .await
}

/// Pushed views until one satisfies `pred`.
async fn until(
    sub: &mut FakeIpcClient,
    pred: impl Fn(&ConversationView) -> bool,
) -> ConversationView {
    within(Duration::from_secs(600), async {
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

/// Starts a take on a subscribed connection; returns the first view pushed after the answer.
async fn start_take(c: &mut FakeIpcClient) -> ConversationView {
    c.tx_request(START).await;
    loop {
        match c.next().await {
            Some(IpcResponse::Conversation(_)) => {}
            Some(IpcResponse::CaptureStarted) => break,
            other => panic!("{other:?}"),
        }
    }
    match within(Duration::from_secs(60), c.next()).await {
        Some(IpcResponse::Conversation(v)) => v,
        other => panic!("{other:?}"),
    }
}

trait TxRequest {
    async fn tx_request(&mut self, r: IpcRequest);
}

impl TxRequest for FakeIpcClient {
    /// Writes the request without reading anything back.
    async fn tx_request(&mut self, r: IpcRequest) {
        self.send_half(&r, true).await;
        self.send_half(&r, false).await;
    }
}

/// Speaks for `ms` milliseconds: one 20 ms chunk every 20 ms of virtual time.
async fn speak(rig: &Rig, ms: u64) {
    for _ in 0..ms / 20 {
        rig.mic.push_all(tone_chunks(1, 0.3));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Lets the agent finish what it is doing (virtual time).
async fn settle() {
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[tokio::test(start_paused = true)]
async fn a_take_ended_with_send_sends_its_transcript() {
    let mut rig = rig_with(MockConfig::default()).start();
    wait_ready(&mut rig.health).await;
    let (mut sub, first) = subscribe(&rig).await;
    assert_eq!(first.activity, UserActivity::Idle);
    assert!(!rig.mic.is_open(), "never open at start");

    let v = start_take(&mut sub).await;
    assert_eq!(v.activity, UserActivity::Recording);
    assert!(rig.mic.is_open());
    speak(&rig, 1000).await;
    assert_eq!(
        ask(&mut sub, end(CaptureThen::Send)).await,
        IpcResponse::CaptureEnded
    );
    let v = until(&mut sub, |v| {
        v.turns.last().is_some_and(|t| t.mine == "pon la serie")
    })
    .await;
    assert_eq!(v.live, "");
    let log = rig.link.log();
    assert!(log.audio_frames > 0, "the take's audio reached the gateway");
    assert_eq!(log.sends, ["pon la serie"]);
    let end_at = log.client_msgs.iter().position(|m| m == "end").unwrap();
    let send_at = log.client_msgs.iter().position(|m| m == "send").unwrap();
    assert!(end_at < send_at, "{:?}", log.client_msgs);
    assert!(!rig.mic.is_open(), "released");
    let s = rig.health.snapshot().stats;
    assert_eq!((rig.mic.opens(), s.captures_opened), (1, 1));
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn review_and_discard_never_send() {
    let mut rig = rig_with(MockConfig::default()).start();
    wait_ready(&mut rig.health).await;
    let (mut sub, _) = subscribe(&rig).await;

    start_take(&mut sub).await;
    speak(&rig, 1000).await;
    assert_eq!(
        ask(&mut sub, end(CaptureThen::Review)).await,
        IpcResponse::CaptureEnded
    );
    let v = until(&mut sub, |v| v.draft.is_some()).await;
    assert_eq!(v.draft.as_deref(), Some("pon la serie"));
    assert_eq!(v.activity, UserActivity::Idle);

    start_take(&mut sub).await;
    speak(&rig, 1000).await;
    assert_eq!(
        ask(&mut sub, end(CaptureThen::Discard)).await,
        IpcResponse::CaptureEnded
    );
    until(&mut sub, |v| v.activity == UserActivity::Idle).await;
    settle().await;

    let log = rig.link.log();
    assert!(
        log.sends.is_empty(),
        "Review and Discard never send: {:?}",
        log.client_msgs
    );
    assert_eq!(log.client_msgs.iter().filter(|m| *m == "end").count(), 2);
    assert_eq!(rig.mic.opens(), 2);
    assert!(rig.health.conversation().turns.is_empty());
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn a_take_under_250_ms_is_discarded_even_with_send() {
    let mut rig = rig_with(MockConfig::default()).start();
    wait_ready(&mut rig.health).await;
    let (mut sub, _) = subscribe(&rig).await;
    start_take(&mut sub).await;
    speak(&rig, 100).await;
    assert_eq!(
        ask(&mut sub, end(CaptureThen::Send)).await,
        IpcResponse::CaptureEnded
    );
    until(&mut sub, |v| v.activity == UserActivity::Idle).await;
    settle().await;
    assert!(rig.link.log().sends.is_empty());
    assert_eq!(rig.health.snapshot().stats.captures_discarded, 1);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn closing_the_connection_closes_and_discards_the_take() {
    let mut rig = rig_with(MockConfig::default()).start();
    wait_ready(&mut rig.health).await;
    let (mut sub, _) = subscribe(&rig).await;
    start_take(&mut sub).await;
    speak(&rig, 1000).await;
    drop(sub);
    eventually(Duration::from_secs(5), || !rig.mic.is_open()).await;
    settle().await;
    let v = rig.health.conversation();
    assert_eq!((v.activity, v.draft), (UserActivity::Idle, None));
    let log = rig.link.log();
    assert!(log.sends.is_empty());
    assert!(
        log.client_msgs.contains(&"end".to_owned()),
        "the utterance is closed"
    );
    assert_eq!(rig.health.snapshot().stats.captures_discarded, 1);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn nothing_heard_for_5_s_cuts_to_review() {
    // No partial while recording (green-house in silence): only the final after `end`.
    let mut rig = rig_with(MockConfig::default()).start();
    wait_ready(&mut rig.health).await;
    let (mut sub, _) = subscribe(&rig).await;
    start_take(&mut sub).await;
    let t0 = tokio::time::Instant::now();
    let watch = async {
        let v = until(&mut sub, |v| v.notice == Some(Notice::SilenceCut)).await;
        (v, t0.elapsed())
    };
    let ((v, cut_after), ()) = tokio::join!(watch, speak(&rig, 7000));
    assert!(
        cut_after >= Duration::from_secs(5) && cut_after < Duration::from_millis(5500),
        "{cut_after:?}"
    );
    assert_ne!(v.activity, UserActivity::Recording);
    assert!(!rig.mic.is_open());
    let v = until(&mut sub, |v| v.draft.is_some()).await;
    assert_eq!(v.draft.as_deref(), Some("pon la serie"));
    assert!(
        rig.link.log().sends.is_empty(),
        "an automatic cut never sends"
    );
    assert_eq!(rig.health.snapshot().stats.captures_silence_cut, 1);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn the_safety_cut_closes_a_take_that_keeps_hearing() {
    let mut b = rig_with(MockConfig {
        partial_every_frames: Some(1),
        ..MockConfig::default()
    });
    // Shorter than the silence cut so the safety net is what fires.
    b.cfg.capture_safety_cut = Duration::from_secs(3);
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let (mut sub, _) = subscribe(&rig).await;
    start_take(&mut sub).await;
    let watch = async { until(&mut sub, |v| v.notice == Some(Notice::SafetyCut)).await };
    let (v, ()) = tokio::join!(watch, speak(&rig, 4000));
    assert!(!rig.mic.is_open());
    assert_eq!(v.live, "", "the live line goes with the take");
    let v = until(&mut sub, |v| v.draft.is_some()).await;
    assert_eq!(v.draft.as_deref(), Some("pon la serie"));
    assert!(rig.link.log().sends.is_empty());
    assert_eq!(rig.health.snapshot().stats.captures_safety_cut, 1);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn the_live_partial_shows_while_recording() {
    let mut rig = rig_with(MockConfig {
        partial_every_frames: Some(5),
        ..MockConfig::default()
    })
    .start();
    wait_ready(&mut rig.health).await;
    let (mut sub, _) = subscribe(&rig).await;
    start_take(&mut sub).await;
    let watch = async { until(&mut sub, |v| v.live == "pon la serie").await };
    let (v, ()) = tokio::join!(watch, speak(&rig, 500));
    assert_eq!(v.activity, UserActivity::Recording);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn a_dropped_link_closes_the_take_and_nothing_reopens_it() {
    let mut rig = rig_with(MockConfig::default()).start();
    wait_ready(&mut rig.health).await;
    let (mut sub, _) = subscribe(&rig).await;
    start_take(&mut sub).await;
    speak(&rig, 500).await;
    rig.link.control(Ctl::Abort);
    let watch = async { until(&mut sub, |v| v.activity == UserActivity::Idle).await };
    let ((), _) = tokio::join!(speak(&rig, 200), watch);
    assert!(!rig.mic.is_open());
    // Reconnects, pushes, more closes: the microphone stays closed.
    wait_state(
        &mut rig.health,
        |v| matches!(v.state, fogar_core::LinkState::Ready { session } if session >= 2),
    )
    .await;
    rig.link.control(Ctl::Push);
    settle().await;
    assert_eq!(rig.mic.opens(), 1);
    assert!(!rig.mic.is_open());
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn refusals_never_touch_the_device() {
    // (mic, mock) -> refusal
    let voiceless = MockConfig {
        voice: false,
        ..MockConfig::default()
    };
    for (name, mic, mock, want) in [
        (
            "not determined",
            FakeMic::not_determined(),
            MockConfig::default(),
            CaptureRefusal::PermissionNotDetermined,
        ),
        (
            "denied",
            FakeMic::denied(),
            MockConfig::default(),
            CaptureRefusal::PermissionDenied,
        ),
        (
            "restricted",
            FakeMic::healthy().with_permission(MicPermission::Restricted),
            MockConfig::default(),
            CaptureRefusal::PermissionDenied,
        ),
        (
            "unknown on a platform with a model",
            FakeMic::healthy().with_permission(MicPermission::Unknown),
            MockConfig::default(),
            CaptureRefusal::PermissionNotDetermined,
        ),
        (
            "no capture",
            FakeMic::minimal(),
            MockConfig::default(),
            CaptureRefusal::NoMicrophone,
        ),
        (
            "no transcriber",
            FakeMic::healthy(),
            voiceless.clone(),
            CaptureRefusal::NoTranscriber,
        ),
    ] {
        let mut b = rig_with(mock);
        b.mic = mic;
        let mut rig = b.start();
        wait_ready(&mut rig.health).await;
        let (mut sub, _) = subscribe(&rig).await;
        assert_eq!(
            ask(&mut sub, START).await,
            IpcResponse::CaptureRefused(want),
            "{name}"
        );
        assert_eq!((rig.mic.opens(), rig.mic.prompts()), (0, 0), "{name}");
        assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
    }
}

#[tokio::test(start_paused = true)]
async fn a_start_while_offline_is_refused() {
    let mut b = rig_with(MockConfig::default());
    b.secrets = std::sync::Arc::new(fogar_testkit::MemorySecretStore::default());
    let mut rig = b.start();
    wait_state(&mut rig.health, |v| {
        v.state == fogar_core::LinkState::NotPaired
    })
    .await;
    let (mut sub, _) = subscribe(&rig).await;
    assert_eq!(
        ask(&mut sub, START).await,
        IpcResponse::CaptureRefused(CaptureRefusal::NotReady)
    );
    assert_eq!(rig.mic.opens(), 0);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn asking_for_the_permission_prompts_once_then_a_take_opens() {
    let mut b = rig_with(MockConfig::default());
    b.mic = FakeMic::not_determined().answering(MicPermission::Granted);
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let (mut sub, first) = subscribe(&rig).await;
    assert_eq!(first.mic, MicPermission::NotDetermined);
    assert_eq!(
        (rig.mic.prompts(), rig.mic.opens()),
        (0, 0),
        "never at start"
    );
    assert_eq!(
        ask(&mut sub, IpcRequest::RequestMicPermission).await,
        IpcResponse::MicPermission(MicPermission::Granted)
    );
    assert_eq!(rig.mic.prompts(), 1);
    assert_eq!(rig.mic.opens(), 0, "allowing is not recording");
    until(&mut sub, |v| v.mic == MicPermission::Granted).await;
    let v = start_take(&mut sub).await;
    assert_eq!(v.activity, UserActivity::Recording);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn uid_only_peers_and_unsubscribed_connections_cannot_open_a_take() {
    let mut b = rig_with(MockConfig::default());
    b.signed_peer = false;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let mut c = rig.ipc.as_ref().unwrap().client().await;
    for req in [
        START,
        IpcRequest::RequestMicPermission,
        end(CaptureThen::Send),
    ] {
        assert_eq!(
            c.request(req).await,
            Some(IpcResponse::Refused {
                reason: "needs the signed Fogar app"
            })
        );
    }
    assert_eq!((rig.mic.opens(), rig.mic.permission_requests()), (0, 0));
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);

    let mut rig = rig_with(MockConfig::default()).start();
    wait_ready(&mut rig.health).await;
    let mut c = rig.ipc.as_ref().unwrap().client().await;
    assert_eq!(
        c.request(START).await,
        Some(IpcResponse::CaptureRefused(CaptureRefusal::NotSubscribed))
    );
    assert_eq!(rig.mic.opens(), 0);
    assert_eq!(rig.health.snapshot().stats.captures_refused_unsubscribed, 1);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn one_take_at_a_time_and_no_text_while_recording() {
    let mut rig = rig_with(MockConfig::default()).start();
    wait_ready(&mut rig.health).await;
    let (mut a, _) = subscribe(&rig).await;
    let (mut b, _) = subscribe(&rig).await;
    start_take(&mut a).await;
    assert_eq!(
        ask(&mut b, START).await,
        IpcResponse::CaptureRefused(CaptureRefusal::Busy)
    );
    assert_eq!(
        ask(&mut a, START).await,
        IpcResponse::CaptureRefused(CaptureRefusal::Busy),
        "the same connection pressing again keeps its take"
    );
    assert!(rig.mic.is_open());
    assert_eq!(
        ask(
            &mut b,
            IpcRequest::SendText {
                text: UserText("hola".into())
            }
        )
        .await,
        IpcResponse::TextRefused(TextRefusal::Recording)
    );
    assert_eq!(rig.mic.opens(), 1);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
    assert!(!rig.mic.is_open(), "stopping the agent closes the take");
}

/// Issue #26: once `CaptureStarted` is answered, no view with another activity may follow
/// while the take is open, even with a turn streaming tokens (pending pushes) at that moment.
#[tokio::test(start_paused = true)]
async fn the_recording_view_is_published_before_capture_started() {
    let mut rig = rig_with(MockConfig {
        tts_frames: 200,
        frame_interval: Duration::from_millis(20),
        ..MockConfig::default()
    })
    .start();
    wait_ready(&mut rig.health).await;
    let (mut sub, _) = subscribe(&rig).await;
    rig.link.control(Ctl::Push);
    until(&mut sub, |v| {
        v.speaking || v.turns.iter().any(|t| !t.reply.is_empty())
    })
    .await;
    sub.tx_request(START).await;
    loop {
        match sub.next().await {
            Some(IpcResponse::Conversation(_)) => {}
            Some(IpcResponse::CaptureStarted) => break,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(
        rig.health.conversation().activity,
        UserActivity::Recording,
        "published before the answer"
    );
    // Every push after the answer carries Recording while the take is open (the cut push
    // turn's turn_end and the chunks keep the agent busy meanwhile).
    let mut pushes = 0;
    let watch = async {
        while let Ok(Some(r)) = tokio::time::timeout(Duration::from_secs(2), sub.next()).await {
            match r {
                IpcResponse::Conversation(v) => {
                    assert_eq!(v.activity, UserActivity::Recording);
                    pushes += 1;
                }
                other => panic!("{other:?}"),
            }
        }
    };
    tokio::join!(watch, speak(&rig, 1000));
    assert!(pushes >= 1, "the recording view itself is pushed");
    assert!(rig.mic.is_open());
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn gateway_traffic_alone_never_opens_the_microphone() {
    let mut rig = rig_with(MockConfig {
        partial_every_frames: Some(1),
        ..MockConfig::default()
    })
    .start();
    wait_ready(&mut rig.health).await;
    let (_sub, _) = subscribe(&rig).await;
    for i in 0..5u64 {
        rig.link.control(Ctl::Push);
        tokio::time::sleep(Duration::from_secs(2)).await;
        if i % 2 == 0 {
            rig.link.control(Ctl::Close {
                code: 1012,
                reason: "restart".into(),
            });
        } else {
            rig.link.control(Ctl::Abort);
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    wait_ready(&mut rig.health).await;
    assert_eq!((rig.mic.opens(), rig.mic.permission_requests()), (0, 0));
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

/// Review M1: another connection of the same signed app cannot end someone else's take; the
/// owner can, and ending again stays idempotent for it.
#[tokio::test(start_paused = true)]
async fn only_the_connection_that_opened_the_take_can_end_it() {
    let mut rig = rig_with(MockConfig::default()).start();
    wait_ready(&mut rig.health).await;
    let (mut a, _) = subscribe(&rig).await;
    let (mut b, _) = subscribe(&rig).await;
    start_take(&mut a).await;
    speak(&rig, 500).await;
    for then in [CaptureThen::Send, CaptureThen::Review, CaptureThen::Discard] {
        assert_eq!(
            ask(&mut b, end(then)).await,
            IpcResponse::Refused {
                reason: "no take on this connection"
            }
        );
    }
    assert!(rig.mic.is_open(), "untouched by the other connection");
    assert_eq!(rig.health.conversation().activity, UserActivity::Recording);
    assert_eq!(
        ask(&mut a, end(CaptureThen::Discard)).await,
        IpcResponse::CaptureEnded
    );
    eventually(Duration::from_secs(5), || !rig.mic.is_open()).await;
    assert_eq!(
        ask(&mut a, end(CaptureThen::Send)).await,
        IpcResponse::CaptureEnded
    );
    settle().await;
    assert!(rig.link.log().sends.is_empty());
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}
