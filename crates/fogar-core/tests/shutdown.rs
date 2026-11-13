//! Ordered graceful shutdown: no leaked tasks (runtime live-task count returns to baseline), the
//! gateway gets a close frame, the mic and speaker are released, IPC served until ingress closes.
#![allow(clippy::disallowed_methods)] // the harness drives tokio's paused clock directly

mod common;
use common::*;
use fogar_core::LinkState;
use fogar_ports::ipc::{IPC_VERSION, IpcRequest, IpcResponse};
use fogar_testkit::MockConfig;
use std::time::Duration;

#[tokio::test(start_paused = true)]
async fn graceful_shutdown_returns_to_the_task_baseline() {
    let baseline = alive_tasks();
    let mut b = RigBuilder::fake(MockConfig {
        tts_frames: 1000,
        frame_interval: Duration::from_millis(20),
        ..MockConfig::default()
    });
    b.with_ipc = true;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    assert!(alive_tasks() > baseline);
    assert!(rig.mic.is_open());

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
    assert!(matches!(
        client.request(IpcRequest::Hello { version: 99 }).await,
        Some(IpcResponse::Refused { .. })
    ));

    // Shut down in the middle of a streaming turn.
    speak(&rig.mic, 10).await;
    eventually(Duration::from_secs(30), || {
        rig.health.snapshot().stats.tts_frames_played > 3
    })
    .await;
    let link = rig.link.clone();
    let mic = rig.mic.clone();
    let speaker = rig.speaker.clone();
    let mut health = rig.health.clone();
    let t0 = tokio::time::Instant::now();
    let report = rig.agent.shutdown(Duration::from_secs(5)).await;
    assert!(report.clean, "{report:?}");
    assert_eq!(report.tasks_left, 0);
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert_eq!(health.wait_link(|_| true).await.state, LinkState::Stopped);
    assert!(!mic.is_open(), "capture released");
    assert_eq!(speaker.snapshot().open_now, 0, "output device released");
    assert_eq!(
        link.log().client_closes,
        1,
        "gateway received a close frame"
    );
    drop(client);
    eventually(Duration::from_secs(5), || link.live_server_tasks() == 0).await;
    eventually(Duration::from_secs(5), || alive_tasks() == baseline).await;
    drop(ipc);
}

#[tokio::test(start_paused = true)]
async fn shutdown_while_parked_or_backing_off_is_prompt() {
    let b = RigBuilder::fake(MockConfig::default());
    b.link.fail_next_connects(u32::MAX);
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

/// Review code#2: a peer that never completes the WebSocket close handshake (gateway hung,
/// network gone) must not make an ordinary stop unclean within the daemon's 4 s budget.
mod hung_close {
    use fogar_ports::gateway::{Frame, GatewayLink, LinkError, LinkEvent, LinkSession};
    use fogar_testkit::FakeGatewayLink;

    #[derive(Clone)]
    pub(super) struct HungClose(pub(super) FakeGatewayLink);
    pub(super) struct HungSess(<FakeGatewayLink as GatewayLink>::Session);

    impl LinkSession for HungSess {
        fn max_frame(&self) -> usize {
            self.0.max_frame()
        }
        async fn send(&mut self, f: Frame) -> Result<(), LinkError> {
            self.0.send(f).await
        }
        async fn recv(&mut self) -> Option<LinkEvent> {
            self.0.recv().await
        }
        async fn close(&mut self, _c: u16, _r: &str) -> Result<(), LinkError> {
            std::future::pending().await
        }
    }

    impl GatewayLink for HungClose {
        type Session = HungSess;
        async fn connect(&self) -> Result<HungSess, LinkError> {
            Ok(HungSess(self.0.connect().await?))
        }
    }
}

#[tokio::test(start_paused = true)]
async fn hung_peer_close_still_fits_the_daemon_budget() {
    let link = hung_close::HungClose(fogar_testkit::FakeGatewayLink::new(MockConfig::default()));
    let mut rig = RigBuilder::with_link(link, &MockConfig::default().client_key).start();
    wait_ready(&mut rig.health).await;
    // fogar-agentd's SHUTDOWN_BUDGET.
    let r = rig.agent.shutdown(Duration::from_secs(4)).await;
    assert!(r.clean, "{r:?}");
}
