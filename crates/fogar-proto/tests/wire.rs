//! Wire vectors (from PROTOCOL-SPEC.md examples), round-trips and fuzz-style property tests.
//! Malformed input must produce errors, never panics.

use fogar_proto::{
    ClientMsg, Handshake, InputMode, OutputMode, ServerMsg, decode_client, decode_server,
    encode_client, encode_handshake, encode_tts_frame, parse_tts_frame,
    server::{
        Capabilities, ErrorMsg, Interrupted, PipelineEvent, Ready, Status, Token, Transcription,
        TurnEnd, TurnErrorReason, TurnStart,
    },
};
use proptest::prelude::*;
use std::borrow::Cow;

#[test]
fn spec_ready_vector_decodes() {
    let j = r#"{"type": "ready", "session_id": "c1:1731500000000", "agent": "main", "input_mode": "audio",
 "output_mode": ["audio", "text", "status"],
 "requested_capabilities": {"barge_in": true, "tts": true, "transcriber": true},
 "live_capabilities": {"barge_in": true, "tts": true, "transcriber": true}, "future_field": 1}"#;
    match decode_server(j).unwrap() {
        ServerMsg::Ready(r) => {
            assert_eq!(r.agent.as_deref(), Some("main"));
            assert_eq!(
                r.output_mode,
                vec![OutputMode::Audio, OutputMode::Text, OutputMode::Status]
            );
            assert!(r.live_capabilities.tts);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn spec_turn_vectors_decode() {
    let msgs = [
        r#"{"type":"status","service":"tts","state":"restored"}"#,
        r#"{"type":"transcription","text":"pon la serie"}"#,
        r#"{"type":"turn_start","turn_id":"t-1","turn_seq":1}"#,
        r#"{"type":"token","turn_id":"t-1","text":"Vale, "}"#,
        r#"{"type":"turn_end","turn_id":"t-1"}"#,
        r#"{"type":"interrupted"}"#,
        r#"{"type":"error","code":"TURN_ERROR","message":"turn_timeout","fatal":false,"turn_id":"t-3"}"#,
        r#"{"type":"pipeline_event","stage":"llm_start","turn":4,"x":[1,2]}"#,
        r#"{"type":"tool_call","turn_id":"t-1","phase":"start","name":"n","tool_call_id":"i","args":{"a":1}}"#,
        r#"{"type":"some_future_message","whatever":true}"#,
    ];
    let decoded: Vec<_> = msgs.iter().map(|m| decode_server(m).unwrap()).collect();
    assert!(
        matches!(&decoded[0], ServerMsg::Status(s) if s.state() == fogar_proto::server::ServiceState::Restored)
    );
    assert!(matches!(&decoded[2], ServerMsg::TurnStart(t) if t.turn_seq == 1));
    assert!(matches!(
        &decoded[5],
        ServerMsg::Interrupted(Interrupted { turn_id: None })
    ));
    assert!(
        matches!(&decoded[6], ServerMsg::Error(e) if e.turn_reason() == Some(TurnErrorReason::TurnTimeout))
    );
    assert!(
        matches!(&decoded[7], ServerMsg::PipelineEvent(p) if p.turn == Some(4) && p.stage == "llm_start")
    );
    assert!(matches!(&decoded[9], ServerMsg::Unknown));
}

#[test]
fn malformed_server_frames_are_errors() {
    for bad in [
        "",
        "null",
        "[]",
        "42",
        "\"ready\"",
        "{}",
        r#"{"type":5}"#,
        r#"{"type":"turn_start","turn_id":"t-1","turn_seq":70000}"#,
        r#"{"type":"token"}"#,
        "{\"type\":\"ready\"",
    ] {
        assert!(decode_server(bad).is_err(), "{bad}");
    }
}

#[test]
fn client_vectors_are_exact() {
    assert_eq!(encode_client(&ClientMsg::End).unwrap(), r#"{"type":"end"}"#);
    assert_eq!(
        encode_client(&ClientMsg::Cancel).unwrap(),
        r#"{"type":"cancel"}"#
    );
    assert_eq!(
        encode_client(&ClientMsg::Keepalive).unwrap(),
        r#"{"type":"keepalive"}"#
    );
    assert_eq!(
        encode_client(&ClientMsg::Send {
            text: "pon \"la\" serie".into()
        })
        .unwrap(),
        r#"{"type":"send","text":"pon \"la\" serie"}"#
    );
    let h = Handshake {
        client_key: "k".into(),
        input_mode: InputMode::Audio,
        output_mode: vec![OutputMode::Audio, OutputMode::Text],
        agent: None,
    };
    assert_eq!(
        encode_handshake(&h).unwrap(),
        r#"{"client_key":"k","input_mode":"audio","output_mode":["audio","text"]}"#
    );
}

#[test]
fn handshake_limit_is_in_chars_and_debug_redacts_key() {
    let key = "ñ".repeat(4000); // 4000 chars, 8000 bytes
    let h = Handshake {
        client_key: Cow::Borrowed(&key),
        input_mode: InputMode::Text,
        output_mode: vec![OutputMode::Text],
        agent: None,
    };
    assert!(
        encode_handshake(&h).is_ok(),
        "limit counts characters, not bytes"
    );
    let key2 = "a".repeat(4100);
    let h2 = Handshake {
        client_key: Cow::Borrowed(&key2),
        ..h.clone()
    };
    assert!(encode_handshake(&h2).is_err());
    let dbg = format!("{h2:?}");
    assert!(!dbg.contains("aaaa") && dbg.contains("redacted"));
}

fn arb_text() -> impl Strategy<Value = String> {
    "\\PC{0,40}"
}

fn arb_server_msg() -> impl Strategy<Value = ServerMsg> {
    prop_oneof![
        (
            arb_text(),
            arb_text(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>()
        )
            .prop_map(|(s, a, b, t, x)| {
                ServerMsg::Ready(Ready {
                    session_id: s,
                    agent: Some(a),
                    input_mode: Some(InputMode::Audio),
                    output_mode: vec![OutputMode::Audio, OutputMode::Text],
                    requested_capabilities: Capabilities {
                        barge_in: b,
                        tts: t,
                        transcriber: x,
                    },
                    live_capabilities: Capabilities {
                        barge_in: x,
                        tts: b,
                        transcriber: t,
                    },
                })
            }),
        arb_text().prop_map(|text| ServerMsg::TranscriptionPartial(Transcription { text })),
        arb_text().prop_map(|text| ServerMsg::Transcription(Transcription { text })),
        (arb_text(), any::<u16>())
            .prop_map(|(turn_id, turn_seq)| ServerMsg::TurnStart(TurnStart { turn_id, turn_seq })),
        (arb_text(), arb_text())
            .prop_map(|(turn_id, text)| ServerMsg::Token(Token { turn_id, text })),
        arb_text().prop_map(|turn_id| ServerMsg::TurnEnd(TurnEnd { turn_id })),
        proptest::option::of(arb_text())
            .prop_map(|turn_id| ServerMsg::Interrupted(Interrupted { turn_id })),
        (arb_text(), arb_text(), proptest::option::of(arb_text())).prop_map(
            |(service, state, code)| {
                ServerMsg::Status(Status {
                    service,
                    state,
                    code,
                    message: None,
                })
            }
        ),
        (
            arb_text(),
            arb_text(),
            any::<bool>(),
            proptest::option::of(arb_text())
        )
            .prop_map(
                |(code, message, fatal, turn_id)| ServerMsg::Error(ErrorMsg {
                    code,
                    message,
                    fatal,
                    turn_id
                })
            ),
        (arb_text(), proptest::option::of(any::<u64>()))
            .prop_map(|(stage, turn)| { ServerMsg::PipelineEvent(PipelineEvent { stage, turn }) }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn server_msgs_round_trip(m in arb_server_msg()) {
        let s = serde_json::to_string(&m).unwrap();
        prop_assert_eq!(decode_server(&s).unwrap(), m);
    }

    #[test]
    fn client_send_round_trips(text in "\\PC{0,200}") {
        let m = ClientMsg::Send { text: Cow::Borrowed(&text) };
        let s = encode_client(&m).unwrap();
        prop_assert_eq!(decode_client(&s).unwrap(), m);
    }

    #[test]
    fn tts_frames_round_trip(seq in any::<u16>(), pcm in proptest::collection::vec(any::<i16>(), 0..4800)) {
        let mut buf = Vec::new();
        encode_tts_frame(seq, &pcm, &mut buf);
        let f = parse_tts_frame(&buf).unwrap();
        prop_assert_eq!(f.turn_seq, seq);
        prop_assert_eq!(f.samples().collect::<Vec<_>>(), pcm);
    }

    #[test]
    fn arbitrary_bytes_never_panic(buf in proptest::collection::vec(any::<u8>(), 0..64)) {
        if let Ok(f) = parse_tts_frame(&buf) {
            prop_assert_eq!(f.sample_count(), (buf.len() - 3) / 2);
            let _ = f.samples().count();
        }
        let _ = fogar_proto::decode_upload_f32le(&buf).count();
    }

    #[test]
    fn arbitrary_text_never_panics(s in "\\PC{0,300}") {
        let _ = decode_server(&s);
        let _ = decode_client(&s);
    }

    #[test]
    fn json_shaped_garbage_never_panics(
        ty in prop_oneof![Just("ready".to_string()), Just("turn_start".to_string()), Just("token".to_string()), Just("turn_end".to_string()), Just("interrupted".to_string()), Just("status".to_string()), Just("error".to_string()), Just("pipeline_event".to_string()), Just("tool_call".to_string()), "[a-z_]{0,12}"],
        k in "[a-z_]{0,10}",
        v in prop_oneof![Just("null".to_string()), Just("1e999".to_string()), Just("-1".to_string()), Just("70000".to_string()), Just("[]".to_string()), Just("{}".to_string()), "\"\\PC{0,8}\""],
    ) {
        let s = format!(r#"{{"type":"{ty}","{k}":{v},"turn_id":{v},"turn_seq":{v},"text":{v}}}"#);
        let _ = decode_server(&s);
    }

    #[test]
    fn classify_close_is_total(code in any::<u16>(), reason in "\\PC{0,40}") {
        let _ = fogar_proto::classify_close(code, &reason);
    }
}

/// Review code#5: any frame with `"type":"ready"` is a `ready`, whatever its other fields look
/// like (older gateways, future fields, nulls, new enum values). Missing/odd fields get defaults.
#[test]
fn ready_decoding_is_tolerant() {
    let cases = [
        r#"{"type":"ready"}"#,
        r#"{"type":"ready","session_id":"c:1","agent":null}"#,
        r#"{"type":"ready","capabilities":{"barge_in":true,"tts":true}}"#,
        r#"{"type":"ready","capabilities":["tts"],"agent":"main"}"#,
        r#"{"type":"ready","output_mode":["audio","video","text"],"input_mode":"video"}"#,
        r#"{"type":"ready","output_mode":"audio","input_mode":null,"session_id":42}"#,
        r#"{"type":"ready","requested_capabilities":[],"live_capabilities":{"tts":"yes"}}"#,
        r#"{"type":"ready","live_capabilities":{"tts":true,"new_cap":true}}"#,
    ];
    for c in cases {
        match decode_server(c) {
            Ok(ServerMsg::Ready(_)) => {}
            other => panic!("{c} -> {other:?}"),
        }
    }
    let Ok(ServerMsg::Ready(r)) = decode_server(
        r#"{"type":"ready","agent":null,"output_mode":["audio","video","text"],"live_capabilities":{"tts":true,"new_cap":true}}"#,
    ) else {
        panic!()
    };
    assert_eq!(r.agent, None);
    assert_eq!(r.output_mode, vec![OutputMode::Audio, OutputMode::Text]);
    assert!(r.live_capabilities.tts);
    assert_eq!(r.input_mode, None);
}
