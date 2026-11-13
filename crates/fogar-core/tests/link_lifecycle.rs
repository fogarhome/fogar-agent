//! Gateway close classification, reconnect with exponential backoff + full jitter, keepalive and
//! the idle close, credentials missing/unavailable. Paused time, in-memory gateway.
#![allow(clippy::disallowed_methods)] // the harness drives tokio's paused clock directly

mod common;
use common::*;
use fogar_core::LinkState;
use fogar_proto::CloseAction;
use fogar_testkit::{MockConfig, gateway::Ctl};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Expect {
    Reconnects,
    Parks(CloseAction),
}

/// Every row of spec §11, plus an unknown code and an abrupt drop (1006).
#[tokio::test(start_paused = true)]
async fn every_close_code_is_classified_and_acted_on() {
    let cases: &[(Option<(u16, &str)>, CloseAction, Expect)] = &[
        (
            Some((1008, "Handshake timeout")),
            CloseAction::RetryBackoff,
            Expect::Reconnects,
        ),
        (
            Some((1008, "Handshake too large")),
            CloseAction::FixClient,
            Expect::Parks(CloseAction::FixClient),
        ),
        (
            Some((
                1008,
                "Handshake invalido. Se esperaba JSON de configuración.",
            )),
            CloseAction::FixClient,
            Expect::Parks(CloseAction::FixClient),
        ),
        (
            Some((1008, "Handshake invalido. Se esperaba texto.")),
            CloseAction::FixClient,
            Expect::Parks(CloseAction::FixClient),
        ),
        (
            Some((1008, "Clave de cliente invalida o inactiva.")),
            CloseAction::Repair,
            Expect::Parks(CloseAction::Repair),
        ),
        (
            Some((1008, "Agent 'x' not permitted for this client.")),
            CloseAction::AgentRefused,
            Expect::Parks(CloseAction::AgentRefused),
        ),
        (
            Some((1008, "Agent 'x' not available.")),
            CloseAction::AgentRefused,
            Expect::Parks(CloseAction::AgentRefused),
        ),
        (
            Some((1008, "revoked")),
            CloseAction::Repair,
            Expect::Parks(CloseAction::Repair),
        ),
        (
            Some((1011, "Servicio crítico no disponible.")),
            CloseAction::RetryBackoff,
            Expect::Reconnects,
        ),
        (
            Some((
                1011,
                "Problema estableciendo microservicios internos del hub.",
            )),
            CloseAction::RetryBackoff,
            Expect::Reconnects,
        ),
        (
            Some((1013, "client too slow")),
            CloseAction::RetryBackoff,
            Expect::Reconnects,
        ),
        (
            Some((1000, "")),
            CloseAction::IdleOrNormal,
            Expect::Reconnects,
        ),
        (
            Some((1012, "")),
            CloseAction::ServerRestart,
            Expect::Reconnects,
        ),
        (
            Some((1009, "")),
            CloseAction::FixClient,
            Expect::Parks(CloseAction::FixClient),
        ),
        (Some((4000, "")), CloseAction::Unknown, Expect::Reconnects),
        (None, CloseAction::RetryBackoff, Expect::Reconnects), // abort -> 1006
    ];
    for &(close, action, expect) in cases {
        let mut rig = RigBuilder::fake(MockConfig::default()).start();
        wait_ready(&mut rig.health).await;
        match close {
            Some((code, reason)) => assert_eq!(rig.link.close_all(code, reason), 1),
            None => assert_eq!(rig.link.control(Ctl::Abort), 1),
        }
        let v = wait_state(&mut rig.health, |v| v.last_close.is_some()).await;
        assert_eq!(v.last_close.map(|c| c.1), Some(action), "{close:?}");
        match expect {
            Expect::Reconnects => {
                wait_state(&mut rig.health, |v| {
                    matches!(v.state, LinkState::Ready { session: 2 })
                })
                .await;
            }
            Expect::Parks(reason) => {
                wait_state(&mut rig.health, |v| v.state == LinkState::Parked { reason }).await;
                // Parked means parked: no reconnect storm for an hour.
                tokio::time::sleep(Duration::from_secs(3600)).await;
                let s = rig.health.snapshot().stats;
                assert_eq!(s.connect_attempts, 1, "{close:?} must not retry");
                assert_eq!(rig.link.log().connections, 1);
            }
        }
        let r = rig.agent.shutdown(Duration::from_secs(5)).await;
        assert!(r.clean, "{close:?}: {r:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn handshake_rejection_parks_with_repair() {
    // Wrong key stored locally: the gateway closes 1008 before `ready`.
    let mut b = RigBuilder::fake(MockConfig::default());
    b.secrets = std::sync::Arc::new(fogar_testkit::MemorySecretStore::with_client_key("wrong"));
    let mut rig = b.start();
    let v = wait_state(&mut rig.health, |v| {
        matches!(v.state, LinkState::Parked { .. })
    })
    .await;
    assert_eq!(
        v.state,
        LinkState::Parked {
            reason: CloseAction::Repair
        }
    );
    assert_eq!(rig.health.snapshot().stats.sessions_started, 0);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn reconnect_uses_capped_exponential_backoff_with_jitter() {
    let b = RigBuilder::fake(MockConfig::default());
    let policy = b.cfg.backoff;
    b.link.fail_next_connects(8);
    let t0 = tokio::time::Instant::now();
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let elapsed = t0.elapsed();
    let s = rig.health.snapshot().stats;
    assert_eq!(s.connect_attempts, 9);
    assert_eq!(s.connect_failures, 8);
    // 8 waits, each in [floor, window(attempt)].
    let max: Duration = (0..8).map(|a| policy.window(a)).sum();
    let min = policy.floor * 8;
    assert!(
        elapsed >= min && elapsed <= max + Duration::from_millis(50),
        "{elapsed:?} not in [{min:?}, {max:?}]"
    );
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn backoff_ceiling_holds_under_a_long_outage() {
    let b = RigBuilder::fake(MockConfig::default());
    let cap = b.cfg.backoff.cap;
    b.link.fail_next_connects(500);
    let t0 = tokio::time::Instant::now();
    let mut rig = b.start();
    // 500 failures: uncapped doubling would overflow; capped, every wait is <= cap.
    within(
        cap * 501,
        rig.health
            .wait_link(|v| matches!(v.state, LinkState::Ready { .. })),
    )
    .await;
    assert_eq!(rig.health.snapshot().stats.connect_attempts, 501);
    let elapsed = t0.elapsed();
    assert!(elapsed <= cap * 500 + Duration::from_secs(1), "{elapsed:?}");
    // Full jitter: the average wait is well below the cap, not pinned at it.
    assert!(elapsed < cap * 400, "{elapsed:?}");
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn keepalive_prevents_the_idle_close() {
    let mut rig = RigBuilder::fake(MockConfig::default()).start(); // gateway idle 300 s, keepalive 120 s
    wait_ready(&mut rig.health).await;
    tokio::time::sleep(Duration::from_secs(3600)).await;
    let s = rig.health.snapshot().stats;
    assert_eq!(s.sessions_started, 1, "no idle close in an hour");
    assert!(
        (29..=31).contains(&s.keepalives_sent),
        "{} keepalives",
        s.keepalives_sent
    );
    assert!(rig.link.log().closes_sent.is_empty());
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn without_keepalive_the_idle_close_is_classified_and_recovered() {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.cfg.keepalive_after = None;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let t0 = tokio::time::Instant::now();
    let v = wait_state(&mut rig.health, |v| v.last_close.is_some()).await;
    let idle = t0.elapsed();
    assert!(
        idle >= Duration::from_secs(300) && idle < Duration::from_secs(301),
        "{idle:?}"
    );
    assert_eq!(v.last_close, Some((1000, CloseAction::IdleOrNormal)));
    wait_state(&mut rig.health, |v| {
        matches!(v.state, LinkState::Ready { session: 2 })
    })
    .await;
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn not_paired_waits_quietly_then_connects_when_a_key_appears() {
    let b = RigBuilder::fake(MockConfig::default());
    b.secrets
        .delete(fogar_ports::secrets::SecretKey::GatewayClientKey)
        .ok();
    use fogar_ports::secrets::SecretStore as _;
    let mut rig = b.start();
    wait_state(&mut rig.health, |v| v.state == LinkState::NotPaired).await;
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(
        rig.health.snapshot().stats.connect_attempts,
        0,
        "no connects without a key"
    );
    rig.secrets
        .put(
            fogar_ports::secrets::SecretKey::GatewayClientKey,
            MockConfig::default().client_key.into(),
        )
        .unwrap();
    wait_ready(&mut rig.health).await;
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
