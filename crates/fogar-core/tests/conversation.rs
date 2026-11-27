//! The conversation over IPC: who may use it and what is refused, never touching the
//! microphone. S4 PR 4a, Task 3: the WebSocket chat cases left with the WebSocket chat; the
//! conversation over `/events` comes back with the store (Task 4).
#![allow(clippy::disallowed_methods)] // the harness drives tokio's paused clock directly
#![allow(clippy::unwrap_used)]

mod common;
use common::*;
use fogar_ports::ipc::{
    CaptureGesture, CaptureRefusal, CaptureThen, ConversationView, IpcRequest, IpcResponse,
    SEND_TEXT_MAX_BYTES, ServiceState, TextRefusal, UserText,
};
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

fn rig_with(mock: MockConfig) -> RigBuilder<fogar_testkit::FakeGatewayLink> {
    let mut b = RigBuilder::fake(mock);
    b.with_ipc = true;
    b
}

/// Nothing but `GET /events` reached the gateway.
fn nothing_sent(rig: &Rig) -> bool {
    rest(&rig.routes).is_empty()
}

#[tokio::test(start_paused = true)]
async fn uid_only_peers_are_refused_even_with_the_pairing_opt_in() {
    let mut b = rig_with(MockConfig::default());
    b.signed_peer = false;
    // The development opt-in for Login/Logout must NOT extend to the conversation.
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
    assert!(nothing_sent(&rig));
    assert_eq!(rig.mic.opens(), 0);
    assert_eq!(rig.mic.permission_requests(), 0);
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
    assert!(nothing_sent(&rig));
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
    tokio::time::sleep(test_config().ipc_idle_timeout * 3).await;
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
