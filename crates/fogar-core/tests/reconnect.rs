//! User-initiated reconnect (IPC `Reconnect`): after the gateway closes 4011 ("missing scope",
//! spec §13) or 4020/4021 the link parks and only the person may bring it
//! back. Paused time, in-memory gateway, IPC through the fake adapter.
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
use fogar_testkit::{FakeIpcClient, MockConfig};
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

/// Closes the live session with `code` and waits until the link parks for `reason`.
async fn park(rig: &mut Rig, code: u16, text: &str, reason: CloseAction) {
    assert_eq!(rig.link.close_all(code, text), 1);
    wait_state(&mut rig.health, |v| v.state == LinkState::Parked { reason }).await;
}

/// No new attempt for an hour.
async fn no_new_attempts(rig: &Rig, attempts: u64) {
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(rig.health.snapshot().stats.connect_attempts, attempts);
}

/// 4011 (5.0 §13): the key stays valid, so it is never deleted; the link parks without retrying
/// and the app asks the person to have the scope granted.
#[tokio::test(start_paused = true)]
async fn close_4011_parks_and_keeps_the_key() {
    let mut b = RigBuilder::fake(MockConfig {
        missing_scope: true,
        ..MockConfig::default()
    });
    b.with_ipc = true;
    b.signed_peer = true;
    let mut rig = b.start();
    wait_state(&mut rig.health, |v| {
        v.state
            == LinkState::Parked {
                reason: CloseAction::MissingScope,
            }
    })
    .await;
    assert_eq!(rig.health.snapshot().link.label(), "parked_missing_scope");
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(rig.health.snapshot().stats.connect_attempts, 1);
    assert!(rig.secrets.contains(SecretKey::GatewayAccount));
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn close_4011_then_reconnect_is_ready_again() {
    let mut rig = rig(true);
    wait_ready(&mut rig.health).await;
    park(&mut rig, 4011, "missing scope", CloseAction::MissingScope).await;
    no_new_attempts(&rig, 1).await;
    let mut c = client(&rig).await;
    assert_eq!(
        c.request(IpcRequest::Reconnect).await,
        Some(IpcResponse::Reconnecting)
    );
    wait_state(&mut rig.health, |v| {
        matches!(v.state, LinkState::Ready { session: 2 })
    })
    .await;
    assert_eq!(rig.health.snapshot().stats.connect_attempts, 2);
    // Ready again: one more click has nothing to do.
    assert!(matches!(
        c.request(IpcRequest::Reconnect).await,
        Some(IpcResponse::Refused { .. })
    ));
    // Refused again: parked again, and the click that ended the first park does not end this one.
    park(&mut rig, 4011, "missing scope", CloseAction::MissingScope).await;
    no_new_attempts(&rig, 2).await;
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn agent_refused_gets_one_attempt_per_reconnect() {
    let mut rig = rig(true);
    wait_ready(&mut rig.health).await;
    park(
        &mut rig,
        4020,
        "agent not permitted",
        CloseAction::AgentRefused,
    )
    .await;
    let mut c = client(&rig).await;
    assert_eq!(
        c.request(IpcRequest::Reconnect).await,
        Some(IpcResponse::Reconnecting)
    );
    // More clicks either coalesce (link still parked) or are refused (it already left). Here
    // this only shows 4020 takes a reconnect; the stale-click rule is covered by the 4011 test.
    for _ in 0..2 {
        assert!(matches!(
            c.request(IpcRequest::Reconnect).await,
            Some(IpcResponse::Reconnecting | IpcResponse::Refused { .. })
        ));
    }
    wait_state(&mut rig.health, |v| {
        matches!(v.state, LinkState::Ready { session: 2 })
    })
    .await;
    no_new_attempts(&rig, 2).await;
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn reconnect_needs_the_signed_app() {
    let mut rig = rig(false);
    wait_ready(&mut rig.health).await;
    park(&mut rig, 4011, "missing scope", CloseAction::MissingScope).await;
    let mut c = client(&rig).await;
    assert!(matches!(
        c.request(IpcRequest::Reconnect).await,
        Some(IpcResponse::Refused { .. })
    ));
    no_new_attempts(&rig, 1).await;
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn reconnect_does_not_retry_a_client_bug() {
    let mut rig = rig(true);
    wait_ready(&mut rig.health).await;
    park(&mut rig, 4000, "invalid handshake", CloseAction::FixClient).await;
    let mut c = client(&rig).await;
    assert!(matches!(
        c.request(IpcRequest::Reconnect).await,
        Some(IpcResponse::Refused { .. })
    ));
    no_new_attempts(&rig, 1).await;
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn close_4003_parks_without_retry() {
    let mut rig = rig(true);
    wait_ready(&mut rig.health).await;
    park(
        &mut rig,
        4003,
        "protocol version not supported",
        CloseAction::UpgradeClient,
    )
    .await;
    assert_eq!(rig.health.snapshot().link.label(), "parked_upgrade_client");
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(rig.health.snapshot().stats.connect_attempts, 1);
    // Only the person lifts it, and one Reconnect is exactly one attempt.
    let mut c = client(&rig).await;
    assert_eq!(
        c.request(IpcRequest::Reconnect).await,
        Some(IpcResponse::Reconnecting)
    );
    wait_state(&mut rig.health, |v| {
        matches!(v.state, LinkState::Ready { session: 2 })
    })
    .await;
    // The gateway answers 4003 again: parked again, no further attempt by itself.
    park(
        &mut rig,
        4003,
        "protocol version not supported",
        CloseAction::UpgradeClient,
    )
    .await;
    no_new_attempts(&rig, 2).await;
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}
