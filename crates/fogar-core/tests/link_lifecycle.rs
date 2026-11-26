//! Gateway close classification, reconnect with exponential backoff + full jitter, keepalive and
//! the idle close, credentials missing/unavailable. Paused time, in-memory gateway.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)] // the harness drives tokio's paused clock directly

mod common;
use common::*;
use fogar_core::LinkState;
use fogar_ports::secrets::{SecretKey, SecretStore as _};
use fogar_proto::CloseAction;
use fogar_testkit::{MockConfig, gateway::Ctl};
use std::time::Duration;

/// (scripted close or `None` = abrupt drop, expected classification, expected reaction)
type Case = (Option<(u16, &'static str)>, CloseAction, Expect);

#[derive(Debug, Clone, Copy, PartialEq)]
enum Expect {
    Reconnects,
    Parks(CloseAction),
    /// Revoked (spec §0): the account is deleted and the link shows "signed out".
    Unpairs,
}

/// Every row of spec §11, plus an unknown code and an abrupt drop (1006).
#[tokio::test(start_paused = true)]
async fn every_close_code_is_classified_and_acted_on() {
    let cases: &[Case] = &[
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
        // 4.x texts: still classified Repair, but only 4010/4200 delete the account (4.x is
        // gone), so they park and keep the token.
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
        // Gateway M19 ("v3", client-protocol §13): the code is the contract.
        (
            Some((4010, "invalid or inactive credential")),
            CloseAction::Repair,
            Expect::Unpairs,
        ),
        (
            Some((4200, "credential revoked")),
            CloseAction::Repair,
            Expect::Unpairs,
        ),
        (
            Some((4020, "agent not permitted")),
            CloseAction::AgentRefused,
            Expect::Parks(CloseAction::AgentRefused),
        ),
        (
            Some((4000, "invalid handshake")),
            CloseAction::FixClient,
            Expect::Parks(CloseAction::FixClient),
        ),
        (
            Some((4011, "missing scope")),
            CloseAction::MissingScope,
            Expect::Parks(CloseAction::MissingScope),
        ),
        (
            Some((4500, "a future refusal")),
            CloseAction::FixClient,
            Expect::Parks(CloseAction::FixClient),
        ),
        (
            Some((4900, "handshake timeout")),
            CloseAction::RetryBackoff,
            Expect::Reconnects,
        ),
        (
            Some((1011, "internal error")),
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
        // A code outside every known range (M19 4000–4999 are classified above).
        (Some((3000, "")), CloseAction::Unknown, Expect::Reconnects),
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
                assert!(
                    rig.secrets.contains(SecretKey::GatewayAccount),
                    "{close:?} keeps the token"
                );
            }
            Expect::Unpairs => {
                let v = wait_state(&mut rig.health, |v| v.state == LinkState::NotPaired).await;
                assert_eq!(v.gateway, None);
                assert!(
                    !rig.secrets.contains(SecretKey::GatewayAccount),
                    "{close:?}"
                );
                tokio::time::sleep(Duration::from_secs(3600)).await;
                assert_eq!(
                    rig.health.snapshot().stats.connect_attempts,
                    1,
                    "{close:?} must not retry"
                );
            }
        }
        let r = rig.agent.shutdown(Duration::from_secs(5)).await;
        assert!(r.clean, "{close:?}: {r:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn handshake_rejection_of_the_token_signs_out() {
    // Wrong token stored locally: the gateway closes 4010 before `ready`: the account is deleted
    // and the link waits for a new sign-in.
    let mut b = RigBuilder::fake(MockConfig::default());
    b.secrets = std::sync::Arc::new(paired_store("wrong"));
    let mut rig = b.start();
    let v = wait_state(&mut rig.health, |v| v.last_close.is_some()).await;
    assert_eq!(v.last_close, Some((4010, CloseAction::Repair)));
    wait_state(&mut rig.health, |v| v.state == LinkState::NotPaired).await;
    assert!(!rig.secrets.contains(SecretKey::GatewayAccount));
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
    rig.secrets.fail_next(1);
    assert_eq!(rig.link.close_all(4200, "revoked"), 1);
    wait_state(&mut rig.health, |v| {
        v.state
            == LinkState::Parked {
                reason: CloseAction::Repair,
            }
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(rig.health.snapshot().stats.connect_attempts, 1);
    assert_eq!(rig.link.log().connections, 1);
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

/// Review code#1 / sec#2: a gateway that answers every session with a "normal" (1000) or
/// "restart" (1012) close right after `ready` must not drive a reconnect storm. Unstable sessions
/// (shorter than `stable_after`) escalate the backoff like any other failure.
#[tokio::test(start_paused = true)]
async fn flapping_normal_or_restart_close_escalates_backoff() {
    for code in [1000u16, 1012] {
        let mut rig = RigBuilder::fake(MockConfig::default()).start();
        let t0 = tokio::time::Instant::now();
        let mut last_attempt = 0;
        for n in 1..=10u64 {
            wait_state(
                &mut rig.health,
                |v| matches!(v.state, LinkState::Ready { session } if session == n),
            )
            .await;
            assert_eq!(rig.link.close_all(code, ""), 1);
            let v = wait_state(&mut rig.health, |v| {
                matches!(v.state, LinkState::Backoff { .. })
            })
            .await;
            let LinkState::Backoff { attempt, .. } = v.state else {
                unreachable!()
            };
            last_attempt = attempt;
        }
        assert_eq!(last_attempt, 10, "close {code}: attempt must escalate");
        // 9 waits with windows 0.5, 1, 2, 4, ... 60 s: full jitter averages far above the old
        // storm (10 x U[0.1, 0.5] s = a few seconds).
        let elapsed = t0.elapsed();
        assert!(
            elapsed > Duration::from_secs(20),
            "close {code}: 10 flapping sessions in only {elapsed:?}"
        );
        assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
    }
}

/// A session that stayed up >= `stable_after` and then got the idle close reconnects promptly
/// (attempt counter reset, first window).
#[tokio::test(start_paused = true)]
async fn stable_session_normal_close_reconnects_promptly() {
    let b = RigBuilder::fake(MockConfig::default());
    let stable = b.cfg.backoff.stable_after;
    let first = b.cfg.backoff.window(0);
    let mut rig = b.start();
    // Escalate first with a few flapping sessions.
    for n in 1..=4u64 {
        wait_state(
            &mut rig.health,
            |v| matches!(v.state, LinkState::Ready { session } if session == n),
        )
        .await;
        rig.link.close_all(1000, "");
    }
    wait_state(&mut rig.health, |v| {
        matches!(v.state, LinkState::Ready { session: 5 })
    })
    .await;
    tokio::time::sleep(stable + Duration::from_secs(1)).await;
    rig.link.close_all(1000, "");
    let v = wait_state(&mut rig.health, |v| {
        matches!(v.state, LinkState::Backoff { .. })
    })
    .await;
    assert!(
        matches!(v.state, LinkState::Backoff { attempt: 1, .. }),
        "{v:?}"
    );
    let t = tokio::time::Instant::now();
    wait_state(&mut rig.health, |v| {
        matches!(v.state, LinkState::Ready { session: 6 })
    })
    .await;
    assert!(t.elapsed() <= first + Duration::from_millis(50));
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}
