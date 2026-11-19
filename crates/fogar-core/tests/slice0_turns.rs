//! Slice-0 acceptance with fakes in paused time: utterance -> turn -> playback, barge-in, push.
#![allow(clippy::disallowed_methods)] // the harness drives tokio's paused clock directly

mod common;
use common::*;
use fogar_proto::CloseAction;
use fogar_testkit::{MockConfig, gateway::Ctl};
use std::time::Duration;

#[tokio::test(start_paused = true)]
async fn utterance_becomes_a_turn_and_audio_is_played() {
    let mut rig = RigBuilder::fake(MockConfig::default()).start();
    wait_ready(&mut rig.health).await;
    speak(&rig.mic, 10).await;
    eventually(Duration::from_secs(30), || {
        rig.health.snapshot().stats.turns_completed == 1
    })
    .await;

    let log = rig.link.log();
    assert!(
        !log.handshake_had_type_field,
        "handshake must not carry `type`"
    );
    assert_eq!(log.last_output_mode, ["audio", "text"]);
    assert_eq!(log.count("end"), 1);
    assert_eq!(log.sends, ["pon la serie"]);
    assert!(
        log.audio_frames >= 10,
        "utterance audio streamed as f32le binary frames"
    );
    assert_eq!(log.audio_bytes, log.audio_frames * 320 * 4);
    let sp = rig.speaker.snapshot();
    assert_eq!(sp.accepted, 5 * 480, "every TTS sample reached the player");
    assert_eq!(
        rig.speaker.played_samples()[1],
        37 - 1000,
        "PCM16 LE decoded intact (mock sample 1 = 37 - 1000)"
    );
    let s = rig.health.snapshot().stats;
    assert_eq!(
        (
            s.turns_started,
            s.tts_frames_played,
            s.tts_frames_stale,
            s.barge_ins
        ),
        (1, 5, 0, 0)
    );
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn barge_in_cancels_the_streaming_turn() {
    let mock = MockConfig {
        tts_frames: 400,
        frame_interval: Duration::from_millis(20),
        ..MockConfig::default()
    };
    let mut rig = RigBuilder::fake(mock).start();
    wait_ready(&mut rig.health).await;
    speak(&rig.mic, 10).await;
    eventually(Duration::from_secs(30), || {
        rig.health.snapshot().stats.tts_frames_played >= 10
    })
    .await;
    rig.speaker.drain();
    // The user talks over the answer.
    speak(&rig.mic, 10).await;
    eventually(Duration::from_secs(60), || {
        rig.health.snapshot().stats.turns_started >= 2
    })
    .await;
    let s = rig.health.snapshot().stats;
    assert_eq!(s.barge_ins, 1);
    let log = rig.link.log();
    assert_eq!(log.count("cancel"), 1, "barge-in sends `cancel`");
    assert_eq!(log.turns_cancelled, 1);
    assert!(
        rig.speaker.snapshot().stops >= 1,
        "playback flushed immediately"
    );
    assert!(
        log.tts_frames_sent < 400,
        "server stopped streaming the cancelled turn"
    );
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn push_turn_is_played_without_a_request() {
    let mut rig = RigBuilder::fake(MockConfig::default()).start();
    wait_ready(&mut rig.health).await;
    assert_eq!(rig.link.control(Ctl::Push), 1);
    eventually(Duration::from_secs(10), || {
        rig.health.snapshot().stats.tts_frames_played == 5
    })
    .await;
    // A push is not "our" completed turn; it is counted as started.
    assert_eq!(rig.health.snapshot().stats.turns_started, 1);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn push_during_a_turn_waits_for_it_instead_of_replacing_it() {
    let mock = MockConfig {
        tts_frames: 50,
        frame_interval: Duration::from_millis(20),
        ..MockConfig::default()
    };
    let mut rig = RigBuilder::fake(mock).start();
    wait_ready(&mut rig.health).await;
    speak(&rig.mic, 10).await;
    eventually(Duration::from_secs(30), || {
        rig.health.snapshot().stats.tts_frames_played >= 1
    })
    .await;
    // The push arrives while the user's answer plays: it goes out after that turn_end.
    assert_eq!(rig.link.control(Ctl::Push), 1);
    eventually(Duration::from_secs(30), || {
        rig.health.snapshot().stats.tts_frames_played == 100
    })
    .await;
    let log = rig.link.log();
    assert_eq!((log.turns_started, log.turns_cancelled), (2, 0));
    assert_eq!(rig.health.snapshot().stats.turns_completed, 1);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn push_while_a_push_is_open_is_dropped() {
    let mut rig = RigBuilder::fake(MockConfig::default()).start();
    wait_ready(&mut rig.health).await;
    assert_eq!(rig.link.control(Ctl::Push), 1);
    assert_eq!(rig.link.control(Ctl::Push), 1);
    tokio::time::sleep(Duration::from_secs(10)).await;
    let log = rig.link.log();
    assert_eq!((log.turns_started, log.pushes_dropped), (1, 1));
    assert_eq!(rig.health.snapshot().stats.tts_frames_played, 5);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn conversation_taken_mid_answer_stops_playback_and_parks() {
    closed_mid_answer(
        4102,
        "conversation opened elsewhere",
        CloseAction::ConversationTaken,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn agent_withdrawn_mid_answer_stops_playback_and_parks() {
    closed_mid_answer(4020, "agent not permitted", CloseAction::AgentRefused).await;
}

#[tokio::test(start_paused = true)]
async fn unreadable_client_config_mid_answer_ends_the_turn_then_retries() {
    closed_mid_answer(1011, "session setup failed", CloseAction::RetryBackoff).await;
}

/// M19 (3.0.1 §13): the open turn gets `turn_end{closed}` before these mid-session closes. The
/// link going down also stops playback, so `turns_cancelled` is what proves the `closed`.
async fn closed_mid_answer(code: u16, reason: &str, action: CloseAction) {
    let mock = MockConfig {
        tts_frames: 400,
        frame_interval: Duration::from_millis(20),
        ..MockConfig::default()
    };
    let mut rig = RigBuilder::fake(mock).start();
    wait_ready(&mut rig.health).await;
    assert_eq!(rig.link.control(Ctl::Push), 1);
    eventually(Duration::from_secs(30), || {
        rig.health.snapshot().stats.tts_frames_played >= 10
    })
    .await;
    assert_eq!(rig.link.close_all(code, reason), 1);
    wait_state(&mut rig.health, |v| v.last_close == Some((code, action))).await;
    if action != CloseAction::RetryBackoff {
        eventually(Duration::from_secs(10), || {
            rig.health.snapshot().link == fogar_core::LinkState::Parked { reason: action }
        })
        .await;
    }
    assert!(rig.speaker.snapshot().stops >= 1, "the tail is not played");
    assert_eq!(rig.link.log().turns_cancelled, 1);
    assert!(rig.agent.shutdown(Duration::from_secs(5)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn minimal_platform_without_audio_still_connects_and_shuts_down() {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.mic = fogar_testkit::FakeMic::minimal();
    b.speaker = fogar_testkit::FakeSpeaker::minimal();
    let (agent, mut h) = start_no_ipc(b);
    wait_ready(&mut h).await;
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(h.snapshot().stats.mic_chunks, 0);
    assert!(agent.shutdown(Duration::from_secs(5)).await.clean);
}
