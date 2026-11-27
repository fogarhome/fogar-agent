//! S4 PR 4a, Task 3 acceptance (paused time, the `/events` hub): the link is ONE `GET /events`
//! stream. Ready brings it up; the 25 s rotation is invisible above the link; a dead or stuck
//! stream is noticed; resuming loses nothing; early ends back off; 401 signs out, 403 parks; an
//! account change starts over; a newer protocol parks; an oversize event asks for a re-fetch.
#![allow(clippy::disallowed_methods, clippy::unwrap_used, clippy::expect_used)] // the harness drives tokio's paused clock directly

mod common;
use common::*;
use fogar_core::LinkState;
use fogar_ports::{
    ipc::{IpcRequest, IpcResponse, LinkPhase},
    secrets::SecretKey,
};
use fogar_proto::CloseAction;
use fogar_testkit::{HttpRoutes, MockConfig, Scope, StreamStep};
use std::time::Duration;

/// `Last-Event-ID` of every `GET /events` so far, oldest first.
fn opens(routes: &HttpRoutes) -> Vec<Option<String>> {
    routes
        .seen()
        .into_iter()
        .filter(|r| r.method == "GET" && r.path.starts_with("/events"))
        .map(|r| r.last_event_id)
        .collect()
}

/// Every request that is not `GET /events`.
fn rest_calls(routes: &HttpRoutes) -> usize {
    routes
        .seen()
        .iter()
        .filter(|r| !(r.method == "GET" && r.path.starts_with("/events")))
        .count()
}

fn rig() -> Rig {
    RigBuilder::fake(MockConfig::default()).start()
}

fn sse(id: Option<u64>, data: &str) -> Vec<u8> {
    match id {
        Some(id) => format!("id: {id}\ndata: {data}\n\n").into_bytes(),
        None => format!("data: {data}\n\n").into_bytes(),
    }
}

const READY: &str = r#"{"type":"ready","protocol":5,"device_id":"dev-mac"}"#;

#[tokio::test(start_paused = true)]
async fn ready_brings_the_link_up() {
    let mut rig = rig();
    let v = wait_ready(&mut rig.health).await;
    assert_eq!(v.state, LinkState::Ready { session: 1 });
    assert_eq!(rig.hub.subscribers(), 1);
    assert_eq!(
        opens(&rig.routes),
        [None],
        "a fresh connect has no Last-Event-ID"
    );
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
    assert_eq!(rig.hub.subscribers(), 0, "the stream is closed on shutdown");
}

/// Review focus 1: N planned rotations (25 s each) keep the phase `Ready`, push no status to the
/// app and make no REST call; each one resumes from the last id.
#[tokio::test(start_paused = true)]
async fn rotation_is_invisible() {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let mut c = rig.ipc.as_ref().unwrap().client().await;
    assert_eq!(
        c.request(IpcRequest::Subscribe).await,
        Some(IpcResponse::Subscribed)
    );
    match within(Duration::from_secs(5), c.next()).await {
        Some(IpcResponse::Event(r)) => assert_eq!(r.phase, LinkPhase::Ready),
        other => panic!("{other:?}"),
    }
    // Every link change, as the composition root would see it.
    let mut h = rig.health.clone();
    let watcher = tokio::spawn(async move {
        h.wait_link(|v| !matches!(v.state, LinkState::Ready { session: 1 }))
            .await
    });
    let rest_before = rest_calls(&rig.routes);
    let pushed = tokio::time::timeout(Duration::from_secs(10 * 25 + 5), c.next()).await;
    assert!(
        pushed.is_err(),
        "a status push during rotations: {pushed:?}"
    );
    assert!(
        !watcher.is_finished(),
        "the link left Ready during a rotation"
    );
    watcher.abort();
    let s = rig.health.snapshot().stats;
    assert!(s.stream_rotations >= 10, "{s:?}");
    assert_eq!(s.sessions_started, 1);
    assert_eq!(rest_calls(&rig.routes), rest_before, "no REST call");
    let o = opens(&rig.routes);
    assert!(o.len() >= 11, "{o:?}");
    assert!(
        o[1..].iter().all(Option::is_some),
        "every rotation resumes: {o:?}"
    );
    assert_eq!(rig.hub.subscribers(), 1, "the old stream is gone");
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// A text typed right when a stream rotates (the old one ended, the new one not ready yet) is
/// accepted and posted: the rotation is invisible to the engine too. Deferred from Task 3.
#[tokio::test(start_paused = true)]
async fn send_during_rotation_gap_is_accepted() {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    // The first stream ends at 25 s; aim at both sides of it and right on it.
    tokio::time::sleep(Duration::from_millis(24_990)).await;
    for _ in 0..4 {
        say(&rig, "ahora").await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    eventually(Duration::from_secs(5), || {
        rig.routes
            .seen()
            .iter()
            .filter(|r| r.method == "POST" && r.path.starts_with("/conversations"))
            .count()
            == 4
    })
    .await;
    let s = rig.health.snapshot().stats;
    assert!(s.stream_rotations >= 1, "{s:?}");
    assert_eq!(s.sessions_started, 1, "{s:?}");
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// Review focus 1: events published before, during and between rotations all arrive, once.
#[tokio::test(start_paused = true)]
async fn rotation_resumes_without_gaps() {
    let mut rig = rig();
    rig.hub.seed_conversation("c-1", "Serie");
    wait_ready(&mut rig.health).await;
    let first = rig.hub.last_id();
    for i in 0..12u64 {
        rig.hub.other_device_send("c-1", "hola");
        // Lands on both sides of the 25 s rotations, also right at them.
        tokio::time::sleep(Duration::from_millis(7_000 + i * 1_100)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    // message + turn_start + 2 tokens + turn_end per send (ids step by 1–3: count, not subtract).
    let published = 12 * 5;
    let s = rig.health.snapshot().stats;
    assert!(s.stream_rotations >= 4, "{s:?}");
    assert_eq!(s.events_received, published, "no gap, no duplicate: {s:?}");
    assert_eq!(s.events_resync, 0);
    // Every resume names an id the link had, never going back.
    let ids: Vec<u64> = opens(&rig.routes)[1..]
        .iter()
        .map(|o| o.as_deref().unwrap().parse().unwrap())
        .collect();
    assert!(ids.windows(2).all(|w| w[0] <= w[1]), "{ids:?}");
    assert!(ids.iter().all(|i| (first..=rig.hub.last_id()).contains(i)));
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// Review focus 2: no byte (not even `: ping`) for `events_read_timeout` -> reconnect.
#[tokio::test(start_paused = true)]
async fn silent_stream_reconnects() {
    let mut rig = rig();
    wait_ready(&mut rig.health).await;
    rig.hub.go_silent();
    tokio::time::sleep(Duration::from_secs(39)).await;
    assert_eq!(opens(&rig.routes).len(), 1, "not before the read timeout");
    within(
        Duration::from_secs(10),
        rig.health
            .wait_link(|v| matches!(v.state, LinkState::Ready { session: 2 })),
    )
    .await;
    assert!(opens(&rig.routes)[1].is_some(), "resumes from the last id");
    assert_eq!(rig.hub.subscribers(), 1, "the silent stream was dropped");
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// Review focus 2: a `chunk()` that never resolves times out (the body is dropped).
#[tokio::test(start_paused = true)]
async fn stuck_chunk_times_out() {
    let mut rig = rig();
    let mut ready = b"retry: 1000\n\n".to_vec();
    ready.extend(sse(Some(41), READY));
    rig.routes.set_stream(
        "GET",
        "/events",
        200,
        vec![StreamStep::Bytes(ready), StreamStep::HoldOpen],
    );
    wait_ready(&mut rig.health).await;
    tokio::time::sleep(Duration::from_secs(39)).await;
    assert_eq!(rig.routes.disconnects(), 0);
    eventually(Duration::from_secs(5), || rig.routes.disconnects() >= 1).await;
    eventually(Duration::from_secs(5), || opens(&rig.routes).len() >= 2).await;
    assert_eq!(opens(&rig.routes)[1].as_deref(), Some("41"));
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// Review focus 3: EOF before `ready`, or a stream that ends at once after it, backs off with
/// jitter (no reconnect storm).
#[tokio::test(start_paused = true)]
async fn early_eof_backs_off() {
    for script in [
        b"retry: 1000\n\n".to_vec(),
        [b"retry: 1000\n\n".to_vec(), sse(Some(41), READY)].concat(),
    ] {
        let rig = rig();
        rig.routes
            .set_stream("GET", "/events", 200, vec![StreamStep::Bytes(script)]);
        tokio::time::sleep(Duration::from_secs(60)).await;
        let n = opens(&rig.routes).len();
        // 0.5, 1, 2, 4, 8, 16, 32 s windows (full jitter, 100 ms floor).
        assert!((3..=12).contains(&n), "{n} opens in 60 s");
        assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
    }
}

/// Review focus 4: 401 signs out (the account is deleted) and is never retried.
#[tokio::test(start_paused = true)]
async fn unauthorized_signs_out() {
    let mut rig = rig();
    wait_ready(&mut rig.health).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    rig.hub.revoke();
    wait_state(&mut rig.health, |v| v.state == LinkState::NotPaired).await;
    assert!(!rig.secrets.contains(SecretKey::GatewayAccount));
    let n = opens(&rig.routes).len();
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(opens(&rig.routes).len(), n, "signed out: no retry");
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// Review focus 4: `403 missing_scope` parks (the token is kept) and is never retried.
#[tokio::test(start_paused = true)]
async fn missing_scope_parks() {
    let b = RigBuilder::fake(MockConfig::default());
    b.hub.missing_scope(Scope::Read);
    let mut rig = b.start();
    let v = wait_state(&mut rig.health, |v| {
        matches!(v.state, LinkState::Parked { .. })
    })
    .await;
    assert_eq!(
        v.state,
        LinkState::Parked {
            reason: CloseAction::MissingScope
        }
    );
    assert!(rig.secrets.contains(SecretKey::GatewayAccount));
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(opens(&rig.routes).len(), 1, "parked: no retry");
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// Signing out and in again starts a fresh stream: no `Last-Event-ID` of the old account.
#[tokio::test(start_paused = true)]
async fn account_change_restarts_the_link() {
    const LOGIN_OK: &str = r#"{"client_id":"c-9","token":"test-client-key","scopes":["conversations:read","conversations:write"],"user":{"id":"u-1","username":"sito","display_name":"Sito","role":"member"},"home":{"id":"home","name":"green-house"}}"#;
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    b.routes.set("POST", "/auth/login", 201, LOGIN_OK);
    b.routes.set("POST", "/auth/logout", 204, "");
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    tokio::time::sleep(Duration::from_secs(60)).await; // rotations: the link holds an id now
    assert!(opens(&rig.routes).last().unwrap().is_some());
    let mut c = rig.ipc.as_ref().unwrap().client().await;
    assert!(matches!(
        c.request(IpcRequest::Logout).await,
        Some(IpcResponse::LoggedOut { .. })
    ));
    wait_state(&mut rig.health, |v| v.state == LinkState::NotPaired).await;
    eventually(Duration::from_secs(5), || rig.hub.subscribers() == 0).await;
    let r = c
        .request(IpcRequest::Login(Box::new(fogar_ports::ipc::LoginForm {
            address: "127.0.0.1".into(),
            username: "sito".into(),
            password: fogar_ports::ipc::Password::parse("pw"),
        })))
        .await;
    assert!(matches!(r, Some(IpcResponse::LoggedIn(_))), "{r:?}");
    within(
        Duration::from_secs(5),
        rig.health
            .wait_link(|v| matches!(v.state, LinkState::Ready { session: 2 })),
    )
    .await;
    assert_eq!(opens(&rig.routes).last().unwrap(), &None, "fresh stream");
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// `ready.protocol` newer than ours: park (the app must be updated), never retry.
#[tokio::test(start_paused = true)]
async fn newer_protocol_parks() {
    let mut rig = rig();
    rig.routes.set_stream(
        "GET",
        "/events",
        200,
        vec![
            StreamStep::Bytes(sse(Some(41), r#"{"type":"ready","protocol":6}"#)),
            StreamStep::HoldOpen,
        ],
    );
    let v = wait_state(&mut rig.health, |v| {
        matches!(v.state, LinkState::Parked { .. })
    })
    .await;
    assert_eq!(
        v.state,
        LinkState::Parked {
            reason: CloseAction::UpgradeClient
        }
    );
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(opens(&rig.routes).len(), 1);
    eventually(Duration::from_secs(5), || rig.routes.disconnects() == 1).await;
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// An event over the reader's cap is skipped and asks the engine to re-fetch; the stream goes
/// on and the next event arrives.
#[tokio::test(start_paused = true)]
async fn oversize_triggers_resync() {
    let mut rig = rig();
    let big = format!(
        r#"{{"type":"token","turn_id":"t","conversation":"c-1","text":"{}"}}"#,
        "x".repeat(fogar_proto::sse::MAX_EVENT_BYTES)
    );
    let token = r#"{"type":"token","turn_id":"t","conversation":"c-1","text":"y"}"#;
    rig.routes.set_stream(
        "GET",
        "/events",
        200,
        vec![
            StreamStep::Bytes(
                [
                    sse(Some(41), READY),
                    sse(Some(42), &big),
                    sse(Some(43), token),
                ]
                .concat(),
            ),
            StreamStep::HoldOpen,
        ],
    );
    wait_ready(&mut rig.health).await;
    eventually(Duration::from_secs(5), || {
        rig.health.snapshot().stats.events_received == 1
    })
    .await;
    let s = rig.health.snapshot().stats;
    assert_eq!((s.events_oversize, s.events_resync), (1, 1), "{s:?}");
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

/// A rotation whose resume point the gateway no longer keeps: still invisible as a link change,
/// but the engine is asked to re-fetch. A stream cut young (< `stable_stream`) is a real `Down`.
#[tokio::test(start_paused = true)]
async fn lost_resume_point_asks_for_a_resync() {
    for (age, sessions) in [(10, 1), (27, 2)] {
        let mut rig = rig();
        rig.hub.seed_conversation("c-1", "Serie");
        wait_ready(&mut rig.health).await;
        tokio::time::sleep(Duration::from_secs(age)).await;
        rig.hub.drop_streams();
        rig.hub.other_device_send("c-1", "x");
        rig.hub.forget_buffer();
        tokio::time::sleep(Duration::from_secs(10)).await;
        let s = rig.health.snapshot().stats;
        assert_eq!(
            (s.sessions_started, s.events_resync),
            (sessions, 1),
            "{age} s: {s:?}"
        );
        assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
    }
}

/// Push audio is never queued to the engine (nothing can sound in 4a), and it is counted.
#[tokio::test(start_paused = true)]
async fn audio_events_are_dropped_by_the_link() {
    let mut rig = rig();
    rig.hub.set_config(|c| c.audio_to_all = true);
    rig.hub.seed_conversation("c-1", "Serie");
    wait_ready(&mut rig.health).await;
    rig.hub.push("c-1", "hola", true);
    eventually(Duration::from_secs(5), || {
        rig.health.snapshot().stats.events_received == 3
    })
    .await;
    assert_eq!(rig.health.snapshot().stats.events_audio_dropped, 1);
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}
