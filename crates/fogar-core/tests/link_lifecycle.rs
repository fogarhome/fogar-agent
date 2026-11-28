//! Reconnect with exponential backoff + full jitter, flapping streams, credentials
//! missing/unavailable. Paused time, the `/events` hub (S4 PR 4a; the WebSocket close-code table
//! left with the WebSocket chat).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)] // the harness drives tokio's paused clock directly

mod common;
use common::*;
use fogar_core::LinkState;
use fogar_ports::secrets::{SecretKey, SecretStore as _};
use fogar_proto::CloseAction;
use fogar_testkit::{MockConfig, StreamStep};
use std::time::Duration;

fn events_opens(rig: &Rig) -> usize {
    rig.routes
        .seen()
        .iter()
        .filter(|r| r.path.starts_with("/events"))
        .count()
}

#[tokio::test(start_paused = true)]
async fn backoff_ceiling_holds_under_a_long_outage() {
    let b = RigBuilder::fake(MockConfig::default());
    let cap = b.cfg.backoff.cap;
    b.routes.set("GET", "/events", 503, "");
    let t0 = tokio::time::Instant::now();
    let rig = b.start();
    // 100 failures: every wait is <= cap; full jitter keeps the average well below it.
    eventually(cap * 101, || events_opens(&rig) >= 101).await;
    let elapsed = t0.elapsed();
    assert!(elapsed <= cap * 100 + Duration::from_secs(1), "{elapsed:?}");
    assert!(elapsed < cap * 80, "{elapsed:?}");
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn reconnect_uses_capped_exponential_backoff_with_jitter() {
    let b = RigBuilder::fake(MockConfig::default());
    let policy = b.cfg.backoff;
    b.routes.set("GET", "/events", 503, "");
    let t0 = tokio::time::Instant::now();
    let rig = b.start();
    eventually(Duration::from_secs(600), || events_opens(&rig) >= 9).await;
    let elapsed = t0.elapsed();
    // 8 waits, each in [floor, window(attempt)].
    let max: Duration = (0..8).map(|a| policy.window(a)).sum();
    let min = policy.floor * 8;
    assert!(
        elapsed >= min && elapsed <= max + Duration::from_millis(50),
        "{elapsed:?} not in [{min:?}, {max:?}]"
    );
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

/// A gateway that ends every stream right after `ready` must not drive a reconnect storm.
#[tokio::test(start_paused = true)]
async fn flapping_streams_escalate_backoff() {
    let rig = RigBuilder::fake(MockConfig::default()).start();
    rig.routes.set_stream(
        "GET",
        "/events",
        200,
        vec![StreamStep::Bytes(
            b"id: 7\ndata: {\"type\":\"ready\",\"protocol\":5}\n\n".to_vec(),
        )],
    );
    let t0 = tokio::time::Instant::now();
    // (The view coalesces: a Ready that lasts no time may never be seen; count sessions.)
    eventually(Duration::from_secs(3600), || {
        rig.health.snapshot().stats.sessions_started >= 10
    })
    .await;
    // 9 waits with windows 0.5, 1, 2, 4, ... 60 s.
    assert!(t0.elapsed() > Duration::from_secs(20), "{:?}", t0.elapsed());
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn not_paired_waits_quietly() {
    let b = RigBuilder::fake(MockConfig::default());
    b.secrets.delete(SecretKey::GatewayAccount).ok();
    let mut rig = b.start();
    wait_state(&mut rig.health, |v| v.state == LinkState::NotPaired).await;
    // A store read while idle would consume this injected failure and show up as Backoff.
    rig.secrets.fail_next(1);
    let mut view = rig.health.clone();
    let changed = tokio::time::timeout(
        Duration::from_secs(24 * 3600),
        view.wait_link(|v| v.state != LinkState::NotPaired),
    )
    .await;
    assert!(changed.is_err(), "unpaired link woke up: {changed:?}");
    assert_eq!(rig.health.snapshot().stats.connect_attempts, 0);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

/// Spec §0 "no reintentes en bucle": a revocation whose account cannot be read (locked
/// keychain) parks with Repair instead of retrying the dead token.
#[tokio::test(start_paused = true)]
async fn revocation_with_an_unreadable_store_parks() {
    let mut rig = RigBuilder::fake(MockConfig::default()).start();
    wait_ready(&mut rig.health).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    rig.secrets.fail_next(1);
    rig.hub.revoke();
    wait_state(&mut rig.health, |v| {
        v.state
            == LinkState::Parked {
                reason: CloseAction::Repair,
            }
    })
    .await;
    let n = rig.hub.stream_opens();
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(rig.hub.stream_opens(), n);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn locked_secret_store_backs_off_and_recovers() {
    let b = RigBuilder::fake(MockConfig::default());
    b.secrets.fail_next(3);
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    assert_eq!(rig.health.snapshot().stats.connect_attempts, 1);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}
