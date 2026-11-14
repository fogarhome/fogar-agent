//! Bounded queues and their written full-policies (`fogar_core::queues`), under a slow uplink, a
//! flooding microphone and a full speaker buffer. Nothing blocks, nothing grows, drops are counted.
#![allow(clippy::disallowed_methods)] // the harness drives tokio's paused clock directly

mod common;
use common::*;
use fogar_core::queues;
use fogar_testkit::{FakeSpeaker, MockConfig, tone_chunks};
use std::time::Duration;

#[tokio::test(start_paused = true)]
async fn slow_uplink_sheds_audio_but_delivers_control() {
    let b = RigBuilder::fake(MockConfig::default());
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    // Every uplink write now takes 200 ms: ~10x slower than real-time audio production.
    rig.link.set_uplink_delay(Some(Duration::from_millis(200)));
    // A 6 s utterance pushed in real time.
    speak(&rig.mic, 300).await;
    eventually(Duration::from_secs(120), || {
        rig.link.log().count("end") == 1
    })
    .await;
    let h = rig.health.snapshot();
    assert!(
        h.stats.uplink_audio_shed > 0,
        "audio must be shed, not queued without bound"
    );
    assert!(
        h.stats.out_queue_hwm <= queues::OUTBOUND as u64,
        "queue never exceeds its capacity"
    );
    assert_eq!(
        h.stats.uplink_control_dropped, 0,
        "`end` (must-deliver) still got through"
    );
    rig.link.set_uplink_delay(None);
    eventually(Duration::from_secs(60), || {
        rig.health.snapshot().stats.turns_completed == 1
    })
    .await;
    eventually(Duration::from_secs(5), || {
        rig.health.snapshot().out_queue_len == 0
    })
    .await;
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn flooding_microphone_never_blocks_the_producer() {
    let mut rig = RigBuilder::fake(MockConfig::default()).start();
    wait_ready(&mut rig.health).await;
    // 10 s of audio in one burst, without yielding: the adapter-side queue (50) overflows.
    let pushed = rig.mic.push_all(tone_chunks(500, 0.3));
    assert_eq!(pushed.len(), 500);
    assert_eq!(rig.mic.dropped(), 500 - queues::MIC_CHUNKS.get() as u64);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        rig.health.snapshot().stats.mic_chunks,
        queues::MIC_CHUNKS.get() as u64
    );
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
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    speak(&rig.mic, 10).await;
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
/// `end` while the link waits for inbound room; before the fix both sat out their 5 s deadlines,
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
        async fn connect(&self) -> Result<FloodSess, LinkError> {
            let inner = self.inner.connect().await?;
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
    let b = RigBuilder::with_link(link, &MockConfig::default().client_key);
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
    speak(&rig.mic, 100).await;
    tokio::time::sleep(Duration::from_secs(15)).await;
    flood.abort();
    inner.set_uplink_delay(None);
    let s = rig.health.snapshot().stats;
    assert_eq!(
        s.uplink_control_dropped, 0,
        "`end` dropped: link and engine waited on each other"
    );
    assert_eq!(s.sessions_started, 1, "session torn down as a local fault");
    eventually(Duration::from_secs(30), || inner.log().count("end") == 1).await;
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}
