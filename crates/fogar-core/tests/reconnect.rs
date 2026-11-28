//! User-initiated reconnect (IPC `Reconnect`): after `/events` answers `403 missing_scope`, a
//! newer protocol, or a request we got wrong, the link parks and only the person may bring it
//! back (and only for the reasons they can settle). Paused time, the `/events` hub, IPC through
//! the fake adapter.
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::unwrap_used,
    clippy::expect_used
)] // the harness drives tokio's paused clock directly

mod common;
use common::*;
use fogar_core::LinkState;
use fogar_ports::{
    ipc::{IPC_VERSION, IpcRequest, IpcResponse},
    secrets::SecretKey,
};
use fogar_proto::CloseAction;
use fogar_testkit::{FakeIpcClient, MockConfig, Scope, StreamStep};
use std::time::Duration;

fn rig(signed: bool) -> Rig {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    b.signed_peer = signed;
    b.start()
}

async fn client(rig: &Rig) -> FakeIpcClient {
    let mut c = rig.ipc.as_ref().unwrap().client().await;
    assert_eq!(
        c.request(IpcRequest::Hello {
            version: IPC_VERSION
        })
        .await,
        Some(IpcResponse::Hello {
            version: IPC_VERSION
        })
    );
    c
}

/// The stream ends and every reopen gets `403 missing_scope`: the link parks.
async fn park_missing_scope(rig: &mut Rig) {
    rig.hub.missing_scope(Scope::Read);
    rig.hub.drop_streams();
    wait_state(&mut rig.health, |v| {
        v.state
            == LinkState::Parked {
                reason: CloseAction::MissingScope,
            }
    })
    .await;
}

/// No new stream for an hour.
async fn no_new_streams(rig: &Rig) {
    let n = rig.hub.stream_opens() + rest_events(rig);
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(rig.hub.stream_opens() + rest_events(rig), n);
}

/// `GET /events` answered by scripted routes (not counted by the hub).
fn rest_events(rig: &Rig) -> usize {
    rig.routes
        .seen()
        .iter()
        .filter(|r| r.path.starts_with("/events"))
        .count()
}

#[tokio::test(start_paused = true)]
async fn missing_scope_parks_and_keeps_the_key() {
    let mut rig = rig(true);
    wait_ready(&mut rig.health).await;
    park_missing_scope(&mut rig).await;
    assert_eq!(rig.health.snapshot().link.label(), "parked_missing_scope");
    no_new_streams(&rig).await;
    assert!(rig.secrets.contains(SecretKey::GatewayAccount));
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn missing_scope_then_reconnect_is_ready_again() {
    let mut rig = rig(true);
    wait_ready(&mut rig.health).await;
    park_missing_scope(&mut rig).await;
    no_new_streams(&rig).await;
    // The admin granted the scope; the person clicks Reconnect.
    rig.hub.restore();
    let mut c = client(&rig).await;
    assert_eq!(
        c.request(IpcRequest::Reconnect).await,
        Some(IpcResponse::Reconnecting)
    );
    wait_state(&mut rig.health, |v| {
        matches!(v.state, LinkState::Ready { session: 2 })
    })
    .await;
    // Ready again: one more click has nothing to do.
    assert!(matches!(
        c.request(IpcRequest::Reconnect).await,
        Some(IpcResponse::Refused { .. })
    ));
    // Refused again: parked again, and the click that ended the first park does not end this one.
    park_missing_scope(&mut rig).await;
    no_new_streams(&rig).await;
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn reconnect_needs_the_signed_app() {
    let mut rig = rig(false);
    wait_ready(&mut rig.health).await;
    park_missing_scope(&mut rig).await;
    let mut c = client(&rig).await;
    assert!(matches!(
        c.request(IpcRequest::Reconnect).await,
        Some(IpcResponse::Refused { .. })
    ));
    no_new_streams(&rig).await;
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

/// Review M1 of Task 3: a 404 on `/events` (a proxy while the gateway restarts) never parks the
/// daemon: it backs off (capped, jittered; Reconnect has nothing to do) and comes back by
/// itself once the gateway answers.
#[tokio::test(start_paused = true)]
async fn events_404_backs_off_until_the_gateway_answers() {
    let mut rig = rig(true);
    rig.routes
        .set("GET", "/events", 404, r#"{"detail":"Not Found"}"#);
    tokio::time::sleep(Duration::from_secs(60)).await;
    let n = rest_events(&rig);
    assert!((3..=12).contains(&n), "{n} opens in 60 s: backed off");
    assert!(matches!(
        rig.health.snapshot().link,
        LinkState::Backoff { .. } | LinkState::Connecting { .. }
    ));
    let mut c = client(&rig).await;
    assert!(matches!(
        c.request(IpcRequest::Reconnect).await,
        Some(IpcResponse::Refused { .. })
    ));
    rig.routes.set_stream(
        "GET",
        "/events",
        200,
        vec![
            StreamStep::Bytes(b"id: 7\ndata: {\"type\":\"ready\",\"protocol\":5}\n\n".to_vec()),
            StreamStep::HoldOpen,
        ],
    );
    wait_ready(&mut rig.health).await;
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

/// `ready.protocol` newer than ours: parked until the person updates (one attempt per click).
#[tokio::test(start_paused = true)]
async fn newer_protocol_parks_without_retry() {
    let newer = || {
        vec![
            StreamStep::Bytes(b"id: 7\ndata: {\"type\":\"ready\",\"protocol\":6}\n\n".to_vec()),
            StreamStep::HoldOpen,
        ]
    };
    let mut rig = rig(true);
    rig.routes.set_stream("GET", "/events", 200, newer());
    let parked = |v: &fogar_core::LinkView| {
        v.state
            == LinkState::Parked {
                reason: CloseAction::UpgradeClient,
            }
    };
    wait_state(&mut rig.health, parked).await;
    assert_eq!(rig.health.snapshot().link.label(), "parked_upgrade_client");
    no_new_streams(&rig).await;
    // The person updated the gateway... which still says 6: one click is one attempt.
    let mut c = client(&rig).await;
    assert_eq!(
        c.request(IpcRequest::Reconnect).await,
        Some(IpcResponse::Reconnecting)
    );
    eventually(Duration::from_secs(5), || rest_events(&rig) == 2).await;
    wait_state(&mut rig.health, parked).await;
    no_new_streams(&rig).await;
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}
