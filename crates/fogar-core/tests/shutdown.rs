//! Ordered graceful shutdown: no leaked tasks (runtime live-task count returns to baseline), the
//! `/events` stream is closed, the mic and speaker are released, IPC served until ingress closes.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)] // the harness drives tokio's paused clock directly

mod common;
use common::*;
use fogar_core::LinkState;
use fogar_ports::ipc::{IPC_VERSION, IpcRequest, IpcResponse};
use fogar_testkit::{MockConfig, StreamStep};
use std::time::Duration;

#[tokio::test(start_paused = true)]
async fn graceful_shutdown_returns_to_the_task_baseline() {
    let baseline = alive_tasks();
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    let mut rig = b.app_on_screen().start();
    rig.hub.seed_conversation("c-1", "Serie");
    wait_ready(&mut rig.health).await;
    assert!(alive_tasks() > baseline);
    assert_eq!(rig.mic.opens(), 0, "S2 Decision 1: no microphone at start");

    let ipc = rig.ipc.clone().unwrap();
    let mut client = ipc.client().await;
    assert_eq!(
        client
            .request(IpcRequest::Hello {
                version: IPC_VERSION
            })
            .await,
        Some(IpcResponse::Hello {
            version: IPC_VERSION
        })
    );
    match client.request(IpcRequest::Status).await {
        Some(IpcResponse::Status(s)) => assert_eq!(s.link, "ready"),
        other => panic!("{other:?}"),
    }
    // A mismatched client learns our version (and decides itself to stop).
    assert_eq!(
        client.request(IpcRequest::Hello { version: 99 }).await,
        Some(IpcResponse::Hello {
            version: IPC_VERSION
        })
    );

    // Shut down while another device's turn streams in.
    rig.hub.set_reply(&["a"; 50], Duration::from_millis(100));
    rig.hub.other_device_send("c-1", "pon la serie");
    eventually(Duration::from_secs(30), || {
        rig.health.snapshot().stats.events_received > 3
    })
    .await;
    let mic = rig.mic.clone();
    let speaker = rig.speaker.clone();
    let mut health = rig.health.clone();
    let t0 = tokio::time::Instant::now();
    let report = rig.agent.shutdown(Duration::from_secs(5)).await;
    assert!(report.clean, "{report:?}");
    assert_eq!(report.tasks_left, 0);
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert_eq!(health.wait_link(|_| true).await.state, LinkState::Stopped);
    assert_eq!(mic.opens(), 0, "the microphone was never opened");
    assert_eq!(speaker.snapshot().open_now, 0, "output device released");
    assert_eq!(rig.hub.subscribers(), 0, "the stream was closed");
    drop(client);
    // The hub's turn driver ends with its reply.
    tokio::time::sleep(Duration::from_secs(10)).await;
    eventually(Duration::from_secs(5), || alive_tasks() == baseline).await;
    drop(ipc);
}

#[tokio::test(start_paused = true)]
async fn shutdown_while_parked_or_backing_off_is_prompt() {
    let b = RigBuilder::fake(MockConfig::default());
    b.routes.set("GET", "/events", 503, "");
    let mut rig = b.start();
    wait_state(
        &mut rig.health,
        |v| matches!(v.state, LinkState::Backoff { attempt, .. } if attempt >= 5),
    )
    .await;
    let t0 = tokio::time::Instant::now();
    let report = rig.agent.shutdown(Duration::from_secs(5)).await;
    assert!(
        report.clean && t0.elapsed() < Duration::from_millis(10),
        "{report:?}"
    );
}

/// A stream that never sends another byte (gateway hung, network gone) must not make an
/// ordinary stop unclean within the daemon's 4 s budget: the stream is just dropped.
#[tokio::test(start_paused = true)]
async fn hung_stream_still_fits_the_daemon_budget() {
    let mut rig = RigBuilder::fake(MockConfig::default()).start();
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
    // fogar-agentd's SHUTDOWN_BUDGET.
    let r = rig.agent.shutdown(Duration::from_secs(4)).await;
    assert!(r.clean, "{r:?}");
    eventually(Duration::from_secs(1), || rig.routes.disconnects() == 1).await;
}
