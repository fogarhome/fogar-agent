//! The mock gateway's S2 behaviour: `turn_progress` only with `status`, live partials while
//! recording, and a gateway without voice.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::field_reassign_with_default
)]

use fogar_ports::gateway::{Frame, GatewayLink, LinkEvent, LinkSession};
use fogar_proto::{ServerMsg, decode_server, server::ProgressPhase};
use fogar_testkit::{FakeGatewayLink, MockConfig};
use std::time::Duration;

/// Connects with `output_mode`, runs `then`, and returns every JSON message until `stop`
/// matches (or 2 s pass).
async fn session(
    cfg: MockConfig,
    output_mode: &str,
    then: &[Frame],
    stop: impl Fn(&ServerMsg) -> bool,
) -> Vec<ServerMsg> {
    let link = FakeGatewayLink::new(cfg);
    let mut s = link.connect(&link.addr()).await.unwrap();
    s.send(Frame::Text(format!(
        r#"{{"client_key":"test-client-key","input_mode":"audio","output_mode":{output_mode}}}"#
    )))
    .await
    .unwrap();
    for f in then {
        let f = match f {
            Frame::Text(t) => Frame::Text(t.clone()),
            Frame::Binary(b) => Frame::Binary(b.clone()),
        };
        s.send(f).await.unwrap();
    }
    let mut got = Vec::new();
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(2), s.recv()).await {
        if let LinkEvent::Frame(Frame::Text(t)) = ev {
            let m = decode_server(&t).unwrap();
            let done = stop(&m);
            got.push(m);
            if done {
                break;
            }
        }
    }
    got
}

fn send(text: &str) -> Frame {
    Frame::Text(format!(r#"{{"type":"send","text":"{text}"}}"#))
}

fn is_end(m: &ServerMsg) -> bool {
    matches!(m, ServerMsg::TurnEnd(_))
}

#[test]
fn progress_only_when_status_is_requested() {
    fogar_testkit::run_async(async {
        let with = session(
            MockConfig::default(),
            r#"["audio","text","status"]"#,
            &[send("hola")],
            is_end,
        )
        .await;
        let phases: Vec<_> = with
            .iter()
            .filter_map(|m| match m {
                ServerMsg::TurnProgress(p) => Some((p.phase, p.tool.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            phases,
            vec![
                (ProgressPhase::Preparing, None),
                (ProgressPhase::Tool, Some("ha_call_service".into())),
                (ProgressPhase::Thinking, None),
            ]
        );
        let without = session(
            MockConfig::default(),
            r#"["audio","text"]"#,
            &[send("hola")],
            is_end,
        )
        .await;
        assert!(without.iter().any(is_end));
        assert!(
            !without
                .iter()
                .any(|m| matches!(m, ServerMsg::TurnProgress(_)))
        );
    });
}

#[test]
fn partials_while_recording_and_silence_by_default() {
    fogar_testkit::run_async(async {
        let audio: Vec<Frame> = (0..10).map(|_| Frame::Binary(vec![0; 1280])).collect();
        let mut cfg = MockConfig::default();
        cfg.partial_every_frames = Some(5);
        let live = session(cfg, r#"["text"]"#, &audio, |m| {
            matches!(m, ServerMsg::TranscriptionPartial(_))
        })
        .await;
        assert!(
            live.iter()
                .any(|m| matches!(m, ServerMsg::TranscriptionPartial(_)))
        );
        let quiet = session(MockConfig::default(), r#"["text"]"#, &audio, |_| false).await;
        assert!(
            !quiet
                .iter()
                .any(|m| matches!(m, ServerMsg::TranscriptionPartial(_))),
            "by default the mock stays quiet while audio arrives (as green-house in silence)"
        );
    });
}

#[test]
fn a_gateway_without_voice_says_so_before_ready() {
    fogar_testkit::run_async(async {
        let mut cfg = MockConfig::default();
        cfg.voice = false;
        cfg.status_before_ready = false;
        let got = session(cfg, r#"["audio","text"]"#, &[], |m| {
            matches!(m, ServerMsg::Ready(_))
        })
        .await;
        let unavailable: Vec<_> = got
            .iter()
            .filter_map(|m| match m {
                ServerMsg::Status(s) if s.state == "unavailable" => Some(s.service.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(unavailable, ["transcriber", "tts"]);
        let Some(ServerMsg::Ready(r)) = got.last() else {
            panic!("no ready")
        };
        assert!(!r.live_capabilities.transcriber && !r.live_capabilities.tts);
    });
}

#[test]
fn without_voice_only_what_was_asked_is_reported_and_audio_is_discarded() {
    fogar_testkit::run_async(async {
        let mut cfg = MockConfig::default();
        cfg.voice = false;
        cfg.status_before_ready = false;
        cfg.partial_every_frames = Some(1);
        let mut frames: Vec<Frame> = (0..5).map(|_| Frame::Binary(vec![0; 1280])).collect();
        frames.push(Frame::Text(r#"{"type":"end"}"#.into()));
        frames.push(send("hola"));
        // audio in, text out: transcriber unavailable only; no transcript; the turn is text only.
        let got = session(cfg.clone(), r#"["text"]"#, &frames, is_end).await;
        let unavailable: Vec<_> = got
            .iter()
            .filter_map(|m| match m {
                ServerMsg::Status(s) if s.state == "unavailable" => Some(s.service.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(unavailable, ["transcriber"]);
        assert!(!got.iter().any(|m| matches!(
            m,
            ServerMsg::TranscriptionPartial(_) | ServerMsg::Transcription(_)
        )));
        assert!(got.iter().any(is_end), "text still works");
    });
}
