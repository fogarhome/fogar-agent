//! Bounded queues and their written full-policies (`fogar_core::queues`), under a slow uplink, a
//! full speaker buffer and a stalled engine. Nothing blocks, nothing grows, drops are counted.
//! (The flooding-microphone case returns with the capture gate in S2 3b: in 3a nothing opens it.)
#![allow(clippy::disallowed_methods)] // the harness drives tokio's paused clock directly

mod common;
use common::*;
use fogar_core::queues;
use fogar_testkit::{FakeSpeaker, MockConfig};
use std::time::Duration;

#[tokio::test(start_paused = true)]
async fn slow_uplink_still_delivers_control() {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    // Every uplink write now takes 200 ms.
    rig.link.set_uplink_delay(Some(Duration::from_millis(200)));
    say(&rig, "pon la serie").await;
    eventually(Duration::from_secs(120), || {
        rig.health.snapshot().stats.turns_completed == 1
    })
    .await;
    let h = rig.health.snapshot();
    assert_eq!(rig.link.log().sends, ["pon la serie"]);
    assert_eq!(
        h.stats.uplink_control_dropped, 0,
        "`send` (must-deliver) got through"
    );
    assert!(h.stats.out_queue_hwm <= queues::OUTBOUND as u64);
    rig.link.set_uplink_delay(None);
    eventually(Duration::from_secs(5), || {
        rig.health.snapshot().out_queue_len == 0
    })
    .await;
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

/// `queues`: engine commands. The engine stalls on a full uplink queue (4 s per write); four UIs
/// keep typing. Every request is answered within `write_timeout` (accepted or "busy", never a
/// wait), and exactly the accepted texts reach the wire: an abandoned command is never run late.
#[tokio::test(start_paused = true)]
async fn stalled_engine_answers_busy_and_never_runs_abandoned_commands() {
    use fogar_ports::ipc::{IpcRequest, IpcResponse, UserText};
    let mut b = RigBuilder::fake(MockConfig {
        tts_frames: 0,
        ..MockConfig::default()
    });
    b.with_ipc = true;
    let write_timeout = b.cfg.write_timeout;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    rig.link.set_uplink_delay(Some(Duration::from_secs(4)));
    let ipc = rig.ipc.clone().unwrap();
    let mut ui = Vec::new();
    for n in 0..queues::IPC_MAX_CONNS {
        let mut c = ipc.client().await;
        ui.push(tokio::spawn(async move {
            let (mut accepted, mut busy) = (0u32, 0u32);
            for i in 0..30 {
                let t0 = tokio::time::Instant::now();
                let text = UserText(format!("texto {n}-{i}"));
                let r = c.request(IpcRequest::SendText { text }).await;
                assert!(
                    t0.elapsed() <= write_timeout,
                    "a request waited {:?}",
                    t0.elapsed()
                );
                match r {
                    Some(IpcResponse::TextAccepted) => accepted += 1,
                    Some(IpcResponse::Refused {
                        reason: "engine busy",
                    }) => busy += 1,
                    other => panic!("{other:?}"),
                }
            }
            (accepted, busy)
        }));
    }
    let (mut accepted, mut busy) = (0, 0);
    for t in ui {
        let (a, b) = t.await.unwrap();
        accepted += a;
        busy += b;
    }
    assert!(busy > 0, "the stall was never felt");
    assert_eq!(
        u64::from(busy),
        rig.health.snapshot().stats.engine_commands_busy
    );
    rig.link.set_uplink_delay(None);
    eventually(Duration::from_secs(600), || {
        rig.health.snapshot().out_queue_len == 0
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let s = rig.health.snapshot().stats;
    assert_eq!(s.uplink_control_dropped, 0);
    assert_eq!(rig.link.log().sends.len(), accepted as usize);
    assert!(s.out_queue_hwm <= queues::OUTBOUND as u64);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn full_speaker_buffer_drops_and_counts_without_stalling_the_session() {
    let mut b = RigBuilder::fake(MockConfig {
        tts_frames: 20,
        ..MockConfig::default()
    });
    b.speaker = FakeSpeaker::healthy()
        .with_capacity(1000)
        .without_recording();
    b.with_ipc = true;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    say(&rig, "pon la serie").await;
    eventually(Duration::from_secs(30), || {
        rig.health.snapshot().stats.turns_completed == 1
    })
    .await;
    let s = rig.health.snapshot().stats;
    assert_eq!(s.tts_frames_played, 20);
    assert_eq!(s.tts_samples_dropped, 20 * 480 - 1000);
    assert_eq!(rig.speaker.snapshot().accepted, 1000);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

/// Review code#6: the link must never block on the inbound queue while it is the only reader of
/// the outbound queue. Slow uplink + busy downlink: the engine waits for outbound room to send
/// (then `end`, now `send`) while the link waits for inbound room; before the fix both sat out their 5 s deadlines,
/// `end` was dropped and the session was torn down as a local fault.
mod busy_downlink {
    use fogar_ports::gateway::{Frame, GatewayLink, LinkError, LinkEvent, LinkSession};
    use fogar_testkit::FakeGatewayLink;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    #[derive(Clone)]
    pub(super) struct FloodLink {
        pub(super) inner: FakeGatewayLink,
        pub(super) rx: Arc<Mutex<Option<mpsc::Receiver<String>>>>,
    }
    pub(super) struct FloodSess {
        inner: <FakeGatewayLink as GatewayLink>::Session,
        rx: Option<mpsc::Receiver<String>>,
    }

    impl LinkSession for FloodSess {
        fn max_frame(&self) -> usize {
            self.inner.max_frame()
        }
        async fn send(&mut self, f: Frame) -> Result<(), LinkError> {
            self.inner.send(f).await
        }
        async fn recv(&mut self) -> Option<LinkEvent> {
            match self.rx.as_mut() {
                Some(rx) => tokio::select! {
                    biased;
                    Some(t) = rx.recv() => Some(LinkEvent::Frame(Frame::Text(t))),
                    ev = self.inner.recv() => ev,
                },
                None => self.inner.recv().await,
            }
        }
        async fn close(&mut self, c: u16, r: &str) -> Result<(), LinkError> {
            self.inner.close(c, r).await
        }
    }

    impl GatewayLink for FloodLink {
        type Session = FloodSess;
        async fn connect(
            &self,
            addr: &fogar_ports::gateway::GatewayAddr,
        ) -> Result<FloodSess, LinkError> {
            let inner = self.inner.connect(addr).await?;
            let rx = self.rx.lock().ok().and_then(|mut g| g.take());
            Ok(FloodSess { inner, rx })
        }
    }
}

#[tokio::test(start_paused = true)]
async fn link_and_engine_never_wait_on_each_other() {
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(20_000);
    let inner = fogar_testkit::FakeGatewayLink::new(MockConfig::default());
    let link = busy_downlink::FloodLink {
        inner: inner.clone(),
        rx: std::sync::Arc::new(std::sync::Mutex::new(Some(rx))),
    };
    let mut b = RigBuilder::with_link(link, &MockConfig::default().client_key);
    b.with_ipc = true;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    inner.set_uplink_delay(Some(Duration::from_millis(200)));
    // Downlink: one status event per millisecond for 15 s (buffered by the transport).
    let flood = tokio::spawn(async move {
        for _ in 0..15_000 {
            if tx
                .send(r#"{"type":"status","service":"tts","state":"restored"}"#.to_owned())
                .await
                .is_err()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });
    say(&rig, "pon la serie").await;
    tokio::time::sleep(Duration::from_secs(15)).await;
    flood.abort();
    inner.set_uplink_delay(None);
    let s = rig.health.snapshot().stats;
    assert_eq!(
        s.uplink_control_dropped, 0,
        "`send` dropped: link and engine waited on each other"
    );
    assert_eq!(s.sessions_started, 1, "session torn down as a local fault");
    eventually(Duration::from_secs(30), || inner.log().count("send") == 1).await;
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}
