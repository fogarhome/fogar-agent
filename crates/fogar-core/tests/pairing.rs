//! S1 acceptance (paused time, fakes): pairing over the IPC, every `PairFailure`, persist before
//! `Paired`, IPC-4 authorization, unpair, discovery, the status subscription and the version
//! handshake.
#![allow(clippy::disallowed_methods, clippy::unwrap_used, clippy::expect_used)] // the harness drives tokio's paused clock directly

mod common;
use common::*;
use fogar_core::LinkState;
use fogar_ports::{
    ipc::{IPC_VERSION, IpcRequest, IpcResponse, LinkPhase, PairCode, PairFailure, StatusReport},
    secrets::SecretKey,
};
use fogar_testkit::{FakeIpcClient, HttpRoutes, MemorySecretStore, MockConfig};
use std::{sync::Arc, time::Duration};

const PAIR_OK: &str = r#"{"client_id":"c-9","name":"Mac de Sito","client_key":"test-client-key","gateway":{"name":"green-house","version":"1.29.0"}}"#;

/// A rig that is NOT paired, with an IPC socket and a Fogar answering `/pair` with 201.
fn unpaired() -> RigBuilder<fogar_testkit::FakeGatewayLink> {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.secrets = Arc::new(MemorySecretStore::default());
    b.with_ipc = true;
    b.routes.set("POST", "/pair", 201, PAIR_OK);
    b
}

fn pair_req(address: &str, code: &str) -> IpcRequest {
    IpcRequest::Pair {
        address: address.into(),
        code: PairCode::parse(code),
    }
}

async fn client(rig: &Rig) -> FakeIpcClient {
    rig.ipc.as_ref().unwrap().client().await
}

fn posts(routes: &HttpRoutes) -> usize {
    routes.seen().iter().filter(|r| r.method == "POST").count()
}

async fn event(c: &mut FakeIpcClient) -> StatusReport {
    match within(Duration::from_secs(600), c.next()).await {
        Some(IpcResponse::Event(r)) => r,
        other => panic!("expected an Event, got {other:?}"),
    }
}

async fn event_until(c: &mut FakeIpcClient, mut pred: impl FnMut(&StatusReport) -> bool) {
    loop {
        if pred(&event(c).await) {
            return;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn pairing_happy_path_persists_then_connects() {
    let mut rig = unpaired().start();
    wait_state(&mut rig.health, |v| v.state == LinkState::NotPaired).await;
    let mut c = client(&rig).await;
    assert_eq!(
        c.request(IpcRequest::Hello {
            version: IPC_VERSION
        })
        .await,
        Some(IpcResponse::Hello {
            version: IPC_VERSION
        })
    );
    assert_eq!(
        c.request(IpcRequest::Subscribe).await,
        Some(IpcResponse::Subscribed)
    );
    let first = event(&mut c).await;
    assert_eq!(
        (first.phase, first.gateway.clone()),
        (LinkPhase::NotPaired, None)
    );

    let mut p = client(&rig).await;
    let r = p.request(pair_req("127.0.0.1:8004", "482 913")).await;
    // Persist-before-ack: the record is in the store the moment `Paired` is read.
    assert!(rig.secrets.contains(SecretKey::GatewayPairing));
    let Some(IpcResponse::Paired(info)) = r else {
        panic!("{r:?}")
    };
    assert_eq!(
        (
            info.address.as_str(),
            info.name.as_str(),
            info.version.as_str(),
            info.client_name.as_str()
        ),
        (
            "http://127.0.0.1:8004",
            "green-house",
            "1.29.0",
            "Mac de Sito"
        )
    );
    // What went over the wire: the well-known probe, then the code (spaces stripped), our name,
    // the GUESSED client type and what we can play.
    let seen = rig.routes.seen();
    assert_eq!(seen[0].path, "/.well-known/fogar-gateway");
    let post = seen.iter().find(|r| r.method == "POST").unwrap();
    assert_eq!(post.path, "/pair");
    let body: serde_json::Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(body["code"], "482913");
    assert_eq!(body["client_type"], "app");
    assert_eq!(body["name"], rig_cfg_name());
    assert_eq!(body["output_mode"], serde_json::json!(["audio", "text"]));

    // The link restarts on the new record and the subscriber sees it.
    ready_promptly(&mut rig.health).await;
    event_until(&mut c, |r| {
        r.phase == LinkPhase::Ready && r.gateway.as_ref() == Some(&info)
    })
    .await;
    let s = p.request(IpcRequest::Status).await;
    assert!(
        matches!(&s, Some(IpcResponse::Status(r)) if r.gateway.as_ref() == Some(&info)),
        "{s:?}"
    );
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// Ready well before the NotPaired re-read (`backoff.cap`, 60 s): only the pairing-change
/// signal can get it there this fast.
async fn ready_promptly(h: &mut fogar_core::HealthProbe) {
    within(
        Duration::from_secs(5),
        h.wait_link(|v| matches!(v.state, LinkState::Ready { .. })),
    )
    .await;
}

fn rig_cfg_name() -> String {
    test_config().client_name
}

#[tokio::test(start_paused = true)]
async fn legacy_well_known_path_is_accepted() {
    let mut b = unpaired();
    b.routes = HttpRoutes::new();
    b.routes.set(
        "GET",
        "/.well-known/jota-gateway",
        200,
        r#"{"service":"jota-gateway","version":"1.20.0","name":"old-house"}"#,
    );
    b.routes.set("POST", "/pair", 201, PAIR_OK);
    let rig = b.start();
    let mut c = client(&rig).await;
    let r = c.request(pair_req("127.0.0.1", "482913")).await;
    assert!(matches!(r, Some(IpcResponse::Paired(_))), "{r:?}");
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// One row per failure that is decided by the request or the gateway's answer.
#[tokio::test(start_paused = true)]
async fn every_pair_failure_is_reported_and_stores_nothing() {
    // (address, code, /pair status or None = no /pair route, well-known service, expected, posts)
    type Row = (
        &'static str,
        &'static str,
        Option<u16>,
        &'static str,
        PairFailure,
        usize,
    );
    let rows: &[Row] = &[
        (
            "u:p@host",
            "482913",
            Some(201),
            "fogar-gateway",
            PairFailure::BadAddress,
            0,
        ),
        // Clear text to a public IP is refused by the transport policy before a byte is sent.
        (
            "8.8.8.8",
            "482913",
            Some(201),
            "fogar-gateway",
            PairFailure::BadAddress,
            0,
        ),
        (
            "127.0.0.1:8004",
            "48291",
            Some(201),
            "fogar-gateway",
            PairFailure::BadCode,
            0,
        ),
        (
            "127.0.0.1:9",
            "482913",
            Some(201),
            "fogar-gateway",
            PairFailure::Unreachable,
            0,
        ),
        (
            "127.0.0.1:8004",
            "482913",
            Some(201),
            "something-else",
            PairFailure::NotAGateway,
            0,
        ),
        (
            "127.0.0.1:8004",
            "482913",
            Some(400),
            "fogar-gateway",
            PairFailure::InvalidCode,
            1,
        ),
        (
            "127.0.0.1:8004",
            "482913",
            Some(422),
            "fogar-gateway",
            PairFailure::GatewayError,
            1,
        ),
        (
            "127.0.0.1:8004",
            "482913",
            Some(500),
            "fogar-gateway",
            PairFailure::GatewayError,
            1,
        ),
        (
            "127.0.0.1:8004",
            "482913",
            Some(503),
            "fogar-gateway",
            PairFailure::GatewayError,
            1,
        ),
        // 201 with a body that is not a pairing reply.
        (
            "127.0.0.1:8004",
            "482913",
            Some(299),
            "fogar-gateway",
            PairFailure::GatewayError,
            1,
        ),
    ];
    for &(address, code, status, service, expected, n_posts) in rows {
        let mut b = unpaired();
        b.routes = HttpRoutes::new();
        b.routes.set(
            "GET",
            "/.well-known/fogar-gateway",
            200,
            format!(r#"{{"service":"{service}","version":"1","name":"x"}}"#),
        );
        match status {
            Some(299) => b.routes.set("POST", "/pair", 201, r#"{"client_id":"c"}"#),
            Some(st) => b.routes.set(
                "POST",
                "/pair",
                st,
                r#"{"detail":"invalid or expired code"}"#,
            ),
            None => {}
        }
        let rig = b.start();
        let mut c = client(&rig).await;
        let r = c.request(pair_req(address, code)).await;
        assert_eq!(
            r,
            Some(IpcResponse::PairFailed(expected)),
            "{address} {code} {status:?}"
        );
        assert_eq!(posts(&rig.routes), n_posts, "{address} {code} {status:?}");
        if matches!(expected, PairFailure::BadCode) || address.contains('@') {
            assert!(rig.routes.seen().is_empty(), "no network for {expected:?}");
        }
        assert!(!rig.secrets.contains(SecretKey::GatewayPairing));
        assert_eq!(rig.health.snapshot().stats.connect_attempts, 0);
        assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
    }
}

#[tokio::test(start_paused = true)]
async fn no_answer_to_pair_is_a_gateway_error_within_the_deadline() {
    let b = unpaired();
    let pair_timeout = b.cfg.pair_timeout;
    b.routes
        .set_delayed("POST", "/pair", 201, PAIR_OK, pair_timeout * 3);
    let rig = b.start();
    let mut c = client(&rig).await;
    let r = within(
        pair_timeout + Duration::from_secs(5),
        c.request(pair_req("127.0.0.1", "482913")),
    )
    .await;
    assert_eq!(r, Some(IpcResponse::PairFailed(PairFailure::GatewayError)));
    assert!(!rig.secrets.contains(SecretKey::GatewayPairing));
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// The key arrived but the store did not keep it (read-back fails): `StorageFailed`, nothing
/// left behind, the link stays unpaired.
#[tokio::test(start_paused = true)]
async fn storage_failure_after_201_is_reported_not_paired() {
    let b = unpaired();
    b.routes
        .set_delayed("POST", "/pair", 201, PAIR_OK, Duration::from_secs(2));
    let mut rig = b.start();
    wait_state(&mut rig.health, |v| v.state == LinkState::NotPaired).await;
    let mut c = client(&rig).await;
    let pending = c.request(pair_req("127.0.0.1", "482913"));
    tokio::pin!(pending);
    // While `/pair` is in flight (past the AlreadyPaired check): the next store read fails.
    let r = tokio::select! {
        r = &mut pending => panic!("answered too early: {r:?}"),
        () = tokio::time::sleep(Duration::from_secs(1)) => {
            rig.secrets.fail_next(1);
            pending.await
        }
    };
    assert_eq!(r, Some(IpcResponse::PairFailed(PairFailure::StorageFailed)));
    assert!(!rig.secrets.contains(SecretKey::GatewayPairing));
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(rig.health.snapshot().link, LinkState::NotPaired);
    assert_eq!(rig.health.snapshot().stats.connect_attempts, 0);
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn already_paired_is_refused_without_network() {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let mut c = client(&rig).await;
    let r = c.request(pair_req("127.0.0.1", "482913")).await;
    assert_eq!(r, Some(IpcResponse::PairFailed(PairFailure::AlreadyPaired)));
    assert!(rig.routes.seen().is_empty());
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn one_pairing_at_a_time() {
    let b = unpaired();
    b.routes
        .set_delayed("POST", "/pair", 201, PAIR_OK, Duration::from_secs(3));
    let rig = b.start();
    let mut first = client(&rig).await;
    let mut second = client(&rig).await;
    let slow = first.request(pair_req("127.0.0.1", "482913"));
    tokio::pin!(slow);
    let busy = tokio::select! {
        r = &mut slow => panic!("answered too early: {r:?}"),
        r = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            second.request(pair_req("127.0.0.1", "111111")).await
        } => r,
    };
    assert_eq!(busy, Some(IpcResponse::PairFailed(PairFailure::Busy)));
    assert!(matches!(slow.await, Some(IpcResponse::Paired(_))));
    assert_eq!(posts(&rig.routes), 1);
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// IPC-4: a peer verified by uid only may read but not pair or unpair.
#[tokio::test(start_paused = true)]
async fn uid_only_peers_cannot_pair_or_unpair() {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    b.signed_peer = false;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let mut c = client(&rig).await;
    for req in [IpcRequest::Unpair, pair_req("127.0.0.1", "482913")] {
        let r = c.request(req.clone()).await;
        assert!(
            matches!(r, Some(IpcResponse::Refused { .. })),
            "{req:?}: {r:?}"
        );
    }
    assert!(rig.routes.seen().is_empty());
    assert!(rig.secrets.contains(SecretKey::GatewayPairing));
    let s = c.request(IpcRequest::Status).await;
    assert!(
        matches!(&s, Some(IpcResponse::Status(r)) if r.phase == LinkPhase::Ready && r.gateway.is_some()),
        "{s:?}"
    );
    // The same request from an unpaired agent is refused too (no gateway contacted).
    let mut b = unpaired();
    b.signed_peer = false;
    let rig2 = b.start();
    let mut c2 = client(&rig2).await;
    let r = c2.request(pair_req("127.0.0.1", "482913")).await;
    assert!(matches!(r, Some(IpcResponse::Refused { .. })), "{r:?}");
    assert!(rig2.routes.seen().is_empty());
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
    assert!(rig2.agent.shutdown(Duration::from_secs(4)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn unpair_deletes_the_record_and_stops_the_link_then_repairing_works() {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    b.routes.set("POST", "/pair", 201, PAIR_OK);
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let mut c = client(&rig).await;
    assert_eq!(
        c.request(IpcRequest::Unpair).await,
        Some(IpcResponse::Unpaired)
    );
    assert!(!rig.secrets.contains(SecretKey::GatewayPairing));
    let v = wait_state(&mut rig.health, |v| v.state == LinkState::NotPaired).await;
    assert_eq!(v.gateway, None);
    eventually(Duration::from_secs(5), || rig.link.log().client_closes >= 1).await;
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(rig.link.log().connections, 1, "unpaired: no reconnect");
    let mut c = client(&rig).await; // the first one was closed for being idle
    let r = c.request(pair_req("127.0.0.1", "482913")).await;
    assert!(matches!(r, Some(IpcResponse::Paired(_))), "{r:?}");
    // Promptly, not at the next 60 s re-read: the pairing change restarts the link.
    within(
        Duration::from_secs(5),
        rig.health
            .wait_link(|v| matches!(v.state, LinkState::Ready { session: 2 })),
    )
    .await;
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// After a revocation the person pairs again from the app and the link comes back.
#[tokio::test(start_paused = true)]
async fn revoked_then_repaired() {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    b.routes.set("POST", "/pair", 201, PAIR_OK);
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    assert_eq!(rig.link.close_all(1008, "revoked"), 1);
    wait_state(&mut rig.health, |v| v.state == LinkState::NotPaired).await;
    assert!(!rig.secrets.contains(SecretKey::GatewayPairing));
    // Past the post-revocation backoff: the link now sits in the 60 s NotPaired wait, so only
    // the pairing-change signal can bring it back within 5 s.
    tokio::time::sleep(Duration::from_secs(90)).await;
    let mut c = client(&rig).await;
    let r = c.request(pair_req("127.0.0.1", "482913")).await;
    assert!(matches!(r, Some(IpcResponse::Paired(_))), "{r:?}");
    // Promptly, not at the next 60 s re-read: the pairing change restarts the link.
    within(
        Duration::from_secs(5),
        rig.health
            .wait_link(|v| matches!(v.state, LinkState::Ready { session: 2 })),
    )
    .await;
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn discover_tries_the_saved_address_then_this_machine() {
    // Unpaired: this machine answers.
    let rig = unpaired().start();
    let mut c = client(&rig).await;
    let r = c.request(IpcRequest::Discover).await;
    let Some(IpcResponse::Discovered(found)) = r else {
        panic!("{r:?}")
    };
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].address.as_str(), found[0].name.as_str()),
        ("http://127.0.0.1:8004", "green-house")
    );
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);

    // Paired at the same address: probed once, not twice.
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    let rig = b.start();
    let mut c = client(&rig).await;
    assert!(
        matches!(c.request(IpcRequest::Discover).await, Some(IpcResponse::Discovered(f)) if f.len() == 1)
    );
    assert_eq!(rig.routes.seen().len(), 1);
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);

    // Nothing Fogar anywhere: empty, both well-known paths asked, within the probe deadline.
    let mut b = unpaired();
    b.routes = HttpRoutes::new();
    let probe = b.cfg.probe_timeout;
    let rig = b.start();
    let mut c = client(&rig).await;
    let r = within(
        probe + Duration::from_millis(100),
        c.request(IpcRequest::Discover),
    )
    .await;
    assert_eq!(r, Some(IpcResponse::Discovered(Vec::new())));
    assert_eq!(rig.routes.seen().len(), 2);
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn subscription_pushes_changes_and_is_exempt_from_the_idle_close() {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    let idle = b.cfg.ipc_idle_timeout;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let mut c = client(&rig).await;
    assert_eq!(
        c.request(IpcRequest::Subscribe).await,
        Some(IpcResponse::Subscribed)
    );
    let first = event(&mut c).await;
    assert_eq!(first.phase, LinkPhase::Ready);
    // Silent for 10 idle periods: still subscribed (no request needed).
    tokio::time::sleep(idle * 10).await;
    assert_eq!(rig.link.close_all(1012, ""), 1);
    event_until(&mut c, |r| r.phase == LinkPhase::Reconnecting).await;
    event_until(&mut c, |r| r.phase == LinkPhase::Ready && r.sessions == 2).await;
    // Requests still work on a subscribed connection.
    assert!(matches!(
        c.request(IpcRequest::Status).await,
        Some(IpcResponse::Status(_))
    ));
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// A subscriber that stops reading cannot hold its slot: the push that misses `write_timeout`
/// closes the connection, and the slot serves a new client.
#[tokio::test(start_paused = true)]
async fn slow_subscriber_is_closed() {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    let write_timeout = b.cfg.write_timeout;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let ipc = rig.ipc.clone().unwrap();
    let mut slow = ipc.client().await;
    assert_eq!(
        slow.request(IpcRequest::Subscribe).await,
        Some(IpcResponse::Subscribed)
    );
    // Plenty of state changes while `slow` reads nothing (its buffer holds 8 messages).
    for session in 1..=6u64 {
        assert_eq!(rig.link.close_all(1012, ""), 1);
        wait_state(
            &mut rig.health,
            |v| matches!(v.state, LinkState::Ready { session: s } if s == session + 1),
        )
        .await;
    }
    tokio::time::sleep(write_timeout * 2).await;
    let mut got = 0;
    loop {
        match within(Duration::from_secs(60), slow.next()).await {
            Some(IpcResponse::Event(_)) => got += 1,
            None => break,
            other => panic!("{other:?}"),
        }
    }
    assert!(got <= 9, "bounded: {got}");
    // Every slot is usable again.
    let mut clients = Vec::new();
    for _ in 0..fogar_core::queues::IPC_MAX_CONNS {
        let mut c = ipc.client().await;
        assert!(matches!(
            c.request(IpcRequest::Status).await,
            Some(IpcResponse::Status(_))
        ));
        clients.push(c);
    }
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn another_ipc_version_is_told_ours_then_refused() {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    let rig = b.start();
    let mut c = client(&rig).await;
    assert_eq!(
        c.request(IpcRequest::Hello { version: 1 }).await,
        Some(IpcResponse::Hello {
            version: IPC_VERSION
        })
    );
    for req in [
        IpcRequest::Status,
        IpcRequest::Subscribe,
        IpcRequest::Unpair,
    ] {
        let r = c.request(req.clone()).await;
        assert!(
            matches!(r, Some(IpcResponse::Refused { .. })),
            "{req:?}: {r:?}"
        );
    }
    assert!(rig.secrets.contains(SecretKey::GatewayPairing));
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}
