//! The single consent point (paused time, fakes + the `/events` hub + in-memory IPC). S4 PR 4a:
//! dictation is off until PR 5 (no link reports a transcriber), so every take is refused; the
//! take cases that need a live transcriber left with the WebSocket and come back in PR 5. What
//! stays: every refusal path leaves `FakeMic::opens() == 0`, and gateway traffic alone never
//! opens the microphone.
#![allow(clippy::disallowed_methods)] // the harness drives tokio's paused clock directly
#![allow(clippy::unwrap_used)]

mod common;
use common::*;
use fogar_ports::audio::MicPermission;
use fogar_ports::ipc::{
    CaptureGesture, CaptureRefusal, CaptureThen, ConversationView, IpcRequest, IpcResponse,
};
use fogar_testkit::{FakeGatewayLink, FakeIpcClient, FakeMic, MockConfig};
use std::time::Duration;

const START: IpcRequest = IpcRequest::CaptureStart {
    gesture: CaptureGesture::Toggle,
};

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
        c.send_half(&req, true).await;
        c.send_half(&req, false).await;
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

#[tokio::test(start_paused = true)]
async fn refusals_never_touch_the_device() {
    for (name, mic, want) in [
        (
            "not determined",
            FakeMic::not_determined(),
            CaptureRefusal::PermissionNotDetermined,
        ),
        (
            "denied",
            FakeMic::denied(),
            CaptureRefusal::PermissionDenied,
        ),
        (
            "restricted",
            FakeMic::healthy().with_permission(MicPermission::Restricted),
            CaptureRefusal::PermissionDenied,
        ),
        (
            "unknown on a platform with a model",
            FakeMic::healthy().with_permission(MicPermission::Unknown),
            CaptureRefusal::PermissionNotDetermined,
        ),
        (
            "no capture",
            FakeMic::minimal(),
            CaptureRefusal::NoMicrophone,
        ),
        // shortcut: S4 PR 4a has no transcriber at all (dictation returns in PR 5).
        (
            "no transcriber",
            FakeMic::healthy(),
            CaptureRefusal::NoTranscriber,
        ),
    ] {
        let mut b = rig_with(MockConfig::default());
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
async fn uid_only_peers_and_unsubscribed_connections_cannot_open_a_take() {
    let mut b = rig_with(MockConfig::default());
    b.signed_peer = false;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let mut c = rig.ipc.as_ref().unwrap().client().await;
    for req in [
        START,
        IpcRequest::RequestMicPermission,
        IpcRequest::CaptureEnd {
            then: CaptureThen::Send,
        },
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
async fn gateway_traffic_alone_never_opens_the_microphone() {
    let mut rig = rig_with(MockConfig::default()).start();
    rig.hub.seed_conversation("c-1", "Serie");
    rig.hub.set_config(|c| c.audio_to_all = true);
    wait_ready(&mut rig.health).await;
    let (_sub, _) = subscribe(&rig).await;
    for i in 0..5u64 {
        rig.hub.push("c-1", "hola", true);
        rig.hub.other_device_send("c-1", "pon la serie");
        tokio::time::sleep(Duration::from_secs(2)).await;
        if i % 2 == 0 {
            rig.hub.drop_streams();
        } else {
            rig.hub.go_silent();
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
    wait_ready(&mut rig.health).await;
    assert_eq!((rig.mic.opens(), rig.mic.permission_requests()), (0, 0));
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}
