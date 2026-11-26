//! Wire vectors (from PROTOCOL-SPEC.md examples), round-trips and fuzz-style property tests.
//! Malformed input must produce errors, never panics.

use fogar_proto::{
    ClientMsg, Handshake, InputMode, OutputMode, ServerMsg, decode_client, decode_server,
    encode_client, encode_handshake, encode_tts_frame, parse_tts_frame,
    server::{
        Capabilities, ErrorMsg, Interrupted, PipelineEvent, ProgressPhase, Ready, Status, Token,
        Transcription, TurnEnd, TurnErrorReason, TurnProgress, TurnStart,
    },
};
use fogar_proto::{CloseAction, classify_close, client::PROTOCOL_VERSION, pairing::WellKnown};
use proptest::prelude::*;
use std::borrow::Cow;

#[test]
fn handshake_carries_protocol_5() {
    let h = Handshake {
        client_key: "k".into(),
        protocol: PROTOCOL_VERSION,
        input_mode: InputMode::Text,
        output_mode: vec![OutputMode::Text],
        agent: None,
    };
    assert_eq!(PROTOCOL_VERSION, 5);
    assert!(encode_handshake(&h).unwrap().contains(r#""protocol":5"#));
}

fn ready_protocol(extra: &str) -> Option<u32> {
    let j = format!(r#"{{"type":"ready","session_id":"s","agent":"main"{extra}}}"#);
    match decode_server(&j) {
        Ok(ServerMsg::Ready(r)) => {
            assert_eq!(r.session_id, "s");
            assert_eq!(r.agent.as_deref(), Some("main"));
            r.protocol
        }
        other => panic!("not ready: {other:?}"),
    }
}

#[test]
fn ready_with_protocol_decodes() {
    assert_eq!(ready_protocol(r#","protocol":5"#), Some(5));
}

#[test]
fn ready_without_protocol_still_decodes() {
    assert_eq!(ready_protocol(""), None);
    assert_eq!(ready_protocol(r#","protocol":null"#), None);
}

#[test]
fn ready_with_bad_protocol_is_none() {
    for bad in [
        r#""x""#,
        "[5]",
        r#"{"a":1}"#,
        "-1",
        "1.5",
        "true",
        "99999999999",
    ] {
        assert_eq!(
            ready_protocol(&format!(r#","protocol":{bad}"#)),
            None,
            "{bad}"
        );
    }
}

#[test]
fn close_4003_is_upgrade_client() {
    assert_eq!(
        classify_close(4003, "protocol version not supported"),
        CloseAction::UpgradeClient
    );
    assert_eq!(classify_close(4003, ""), CloseAction::UpgradeClient);
}

#[test]
fn new_turn_error_reasons_are_known() {
    let cases = [
        (
            "orchestrator_auth_failed",
            TurnErrorReason::OrchestratorAuthFailed,
        ),
        (
            "orchestrator_model_unavailable",
            TurnErrorReason::OrchestratorModelUnavailable,
        ),
        (
            "orchestrator_rate_limited",
            TurnErrorReason::OrchestratorRateLimited,
        ),
        ("orchestrator_timeout", TurnErrorReason::OrchestratorTimeout),
        (
            "orchestrator_agent_unavailable",
            TurnErrorReason::OrchestratorAgentUnavailable,
        ),
        ("made_up", TurnErrorReason::Other),
    ];
    for (m, want) in cases {
        let e = ErrorMsg {
            code: "TURN_ERROR".into(),
            message: m.into(),
            fatal: false,
            turn_id: None,
        };
        assert_eq!(e.turn_reason(), Some(want), "{m}");
    }
}

#[test]
fn well_known_with_and_without_name() {
    let old: WellKnown =
        serde_json::from_str(r#"{"service":"fogar-gateway","version":"4.9","name":"Casa"}"#)
            .unwrap();
    assert_eq!(old.name.as_deref(), Some("Casa"));
    let new: WellKnown =
        serde_json::from_str(r#"{"service":"fogar-gateway","version":"5.0","instance_id":"i1"}"#)
            .unwrap();
    assert_eq!(new.name, None);
    assert_eq!(new.version, "5.0");
}

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
        protocol: PROTOCOL_VERSION,
        input_mode: InputMode::Audio,
        output_mode: vec![OutputMode::Audio, OutputMode::Text],
        agent: None,
    };
    assert_eq!(
        encode_handshake(&h).unwrap(),
        r#"{"token":"k","protocol":5,"input_mode":"audio","output_mode":["audio","text"]}"#
    );
}

#[test]
fn handshake_limit_is_in_chars_and_debug_redacts_key() {
    let key = "ñ".repeat(4000); // 4000 chars, 8000 bytes
    let h = Handshake {
        client_key: Cow::Borrowed(&key),
        protocol: PROTOCOL_VERSION,
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
                    protocol: None,
                })
            }),
        arb_text().prop_map(|text| ServerMsg::TranscriptionPartial(Transcription { text })),
        arb_text().prop_map(|text| ServerMsg::Transcription(Transcription { text })),
        (arb_text(), any::<u16>()).prop_map(|(turn_id, turn_seq)| ServerMsg::TurnStart(
            TurnStart {
                turn_id,
                turn_seq,
                origin: None,
                conversation: None
            }
        )),
        (arb_text(), arb_text())
            .prop_map(|(turn_id, text)| ServerMsg::Token(Token { turn_id, text })),
        arb_text().prop_map(|turn_id| ServerMsg::TurnEnd(TurnEnd {
            turn_id,
            reason: None
        })),
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
        (
            arb_text(),
            proptest::option::of(any::<u64>()),
            proptest::option::of(any::<u64>())
        )
            .prop_map(|(stage, turn, elapsed_ms)| {
                ServerMsg::PipelineEvent(PipelineEvent {
                    stage,
                    turn,
                    elapsed_ms,
                })
            }),
        (
            arb_text(),
            prop_oneof![
                Just(ProgressPhase::Preparing),
                Just(ProgressPhase::Thinking),
                Just(ProgressPhase::Tool),
                Just(ProgressPhase::Working)
            ],
            proptest::option::of("[A-Za-z0-9_.:-]{1,128}")
        )
            .prop_map(
                |(turn_id, phase, tool)| ServerMsg::TurnProgress(TurnProgress {
                    turn_id,
                    phase,
                    tool
                })
            ),
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

/// Review sec#7 (LOG-1): `{:?}` of server messages never prints transcripts, reply tokens or
/// tool results (they are user content and end up in logs/panic messages otherwise).
#[test]
fn debug_output_redacts_user_content() {
    for (frame, secret) in [
        (
            r#"{"type":"transcription","text":"my bank PIN is 4321"}"#,
            "4321",
        ),
        (
            r#"{"type":"transcription_partial","text":"my bank PIN is 4321"}"#,
            "4321",
        ),
        (
            r#"{"type":"token","turn_id":"t-1","text":"your PIN is 4321"}"#,
            "4321",
        ),
        (
            r#"{"type":"tool_call","turn_id":"t-1","phase":"result","name":"n","tool_call_id":"i","result":"PIN 4321"}"#,
            "4321",
        ),
    ] {
        let m = decode_server(frame).unwrap();
        let dbg = format!("{m:?} {m:#?}");
        assert!(!dbg.contains(secret), "{dbg}");
        assert!(dbg.contains("redacted"), "{dbg}");
    }
}

/// Gateway M19 ("v3", client-protocol §5, §12): new fields decode; unknown values stay lenient.
#[test]
fn m19_turn_vectors_decode() {
    use fogar_proto::server::{TurnEndReason, TurnOrigin};
    let start = decode_server(
        r#"{"type":"turn_start","turn_id":"t-2","turn_seq":2,"conversation":"0b9d6c1e","origin":"push"}"#,
    )
    .unwrap();
    match start {
        ServerMsg::TurnStart(ts) => {
            assert_eq!(ts.origin, Some(TurnOrigin::Push));
            assert_eq!(ts.conversation.as_deref(), Some("0b9d6c1e"));
        }
        other => panic!("{other:?}"),
    }
    let start = decode_server(
        r#"{"type":"turn_start","turn_id":"t-3","turn_seq":3,"conversation":null,"origin":"robot"}"#,
    )
    .unwrap();
    assert!(
        matches!(start, ServerMsg::TurnStart(ts) if ts.origin.is_none() && ts.conversation.is_none())
    );
    for (raw, want) in [
        ("done", TurnEndReason::Done),
        ("error", TurnEndReason::Error),
        ("cancelled", TurnEndReason::Cancelled),
        ("interrupted", TurnEndReason::Interrupted),
        ("replaced", TurnEndReason::Replaced),
        ("timeout", TurnEndReason::Timeout),
        ("closed", TurnEndReason::Closed),
        ("a-future-reason", TurnEndReason::Error),
    ] {
        let m = decode_server(&format!(
            r#"{{"type":"turn_end","turn_id":"t-1","reason":"{raw}"}}"#
        ))
        .unwrap();
        assert!(
            matches!(m, ServerMsg::TurnEnd(te) if te.reason == Some(want)),
            "{raw}"
        );
    }
    let m = decode_server(r#"{"type":"turn_end","turn_id":"t-1","reason":7}"#).unwrap();
    assert!(matches!(m, ServerMsg::TurnEnd(te) if te.reason.is_none()));
    // ready with the M19 conversation object, and turn_progress (not requested by us): no break.
    assert!(matches!(
        decode_server(r#"{"type":"ready","session_id":"c:1","agent":"main","conversation":{"id":"x","agent":"main"}}"#).unwrap(),
        ServerMsg::Ready(_)
    ));
    assert_eq!(
        decode_server(r#"{"type":"turn_progress","turn_id":"t-1","phase":"thinking","tool":null}"#)
            .unwrap(),
        ServerMsg::TurnProgress(TurnProgress {
            turn_id: "t-1".into(),
            phase: ProgressPhase::Thinking,
            tool: None
        })
    );
    // Tolerant: an unknown or odd phase is «working»; a missing tool is None.
    for (raw, phase) in [
        (
            r#"{"type":"turn_progress","turn_id":"t-2","phase":"dreaming"}"#,
            ProgressPhase::Working,
        ),
        (
            r#"{"type":"turn_progress","turn_id":"t-2","phase":7,"tool":3}"#,
            ProgressPhase::Working,
        ),
        (
            r#"{"type":"turn_progress","turn_id":"t-2"}"#,
            ProgressPhase::Working,
        ),
        (
            r#"{"type":"turn_progress","turn_id":"t-2","phase":"tool","tool":"ha_call_service"}"#,
            ProgressPhase::Tool,
        ),
    ] {
        let ServerMsg::TurnProgress(p) = decode_server(raw).unwrap() else {
            panic!("{raw}")
        };
        assert_eq!(p.phase, phase, "{raw}");
    }
    // A tool "name" that is not an identifier (or is too long) never reaches the UI.
    let long = "a".repeat(129);
    for bad in ["con espacios", "", "x\"y", long.as_str()] {
        let raw =
            serde_json::json!({"type":"turn_progress","turn_id":"t","phase":"tool","tool":bad})
                .to_string();
        let ServerMsg::TurnProgress(p) = decode_server(&raw).unwrap() else {
            panic!("{raw}")
        };
        assert_eq!(p.tool, None, "{bad:?}");
    }
    assert!(matches!(
        decode_server(
            r#"{"type":"pipeline_event","stage":"session_end","elapsed_ms":10008,"turn":0}"#
        )
        .unwrap(),
        ServerMsg::PipelineEvent(PipelineEvent {
            elapsed_ms: Some(10008),
            ..
        })
    ));
}

// ---- 5.0 account protocol (gateway 290efa0, M22): CERTAIN (5.0, M22) ----

use fogar_proto::account::{ApiError, LoginRequest, LoginResponse, Me, Scope, UpdateMe};
use fogar_proto::conversations::{Agents, Conversation, CreateConversation, History, Role};
use fogar_proto::pairing::{ClientType, PairResponse};

#[test]
fn handshake_sends_token_not_client_key() {
    let h = Handshake {
        client_key: "k".into(),
        protocol: PROTOCOL_VERSION,
        input_mode: InputMode::Text,
        output_mode: vec![OutputMode::Text],
        agent: None,
    };
    let j = encode_handshake(&h).unwrap();
    assert!(j.contains(r#""token":"k""#), "{j}");
    assert!(!j.contains("client_key"), "{j}");
}

#[test]
fn pair_response_token_and_scopes() {
    let r: PairResponse = serde_json::from_str(
        r#"{"client_id":"c-1","name":"Mac","token":"sk-1","scopes":["conversations:write"],"gateway":{"name":"casa","version":"5.0.0"}}"#,
    )
    .unwrap();
    assert_eq!(r.client_key, "sk-1");
    assert_eq!(r.scopes, vec!["conversations:write".to_string()]);
    let r: PairResponse = serde_json::from_str(
        r#"{"client_id":"c-1","name":"Mac","token":"sk-1","gateway":{"name":"casa","version":"5.0.0"}}"#,
    )
    .unwrap();
    assert!(r.scopes.is_empty());
    // 4.x is dropped: the old field name is not a credential any more.
    assert!(
        serde_json::from_str::<PairResponse>(
            r#"{"client_id":"c-1","name":"Mac","client_key":"sk-1","gateway":{"name":"casa","version":"1.29.0"}}"#
        )
        .is_err()
    );
}

#[test]
fn scope_other_is_never_sent() {
    let s: Scope = serde_json::from_str(r#""home:future""#).unwrap();
    assert_eq!(s, Scope::Other);
    assert!(serde_json::to_string(&Scope::Other).is_err());
    let req = LoginRequest {
        username: "ana".into(),
        password: "pw".into(),
        device_name: "Mac".into(),
        client_type: None,
        capabilities: None,
        scopes: Some(vec![Scope::ConversationsWrite, Scope::Other]),
    };
    assert!(serde_json::to_string(&req).is_err());
    for (s, w) in [
        (Scope::ConversationsRead, "conversations:read"),
        (Scope::ConversationsWrite, "conversations:write"),
        (Scope::HomeRead, "home:read"),
        (Scope::HomeAdmin, "home:admin"),
    ] {
        assert_eq!(serde_json::to_string(&s).unwrap(), format!("\"{w}\""));
        assert_eq!(
            serde_json::from_str::<Scope>(&format!("\"{w}\"")).unwrap(),
            s
        );
    }
}

#[test]
fn close_4011_is_missing_scope() {
    assert_eq!(
        classify_close(4011, "missing scope"),
        CloseAction::MissingScope
    );
    assert_eq!(classify_close(4011, ""), CloseAction::MissingScope);
}

#[test]
fn close_4102_is_fix_client() {
    assert_eq!(
        classify_close(4102, "conversation opened elsewhere"),
        CloseAction::FixClient
    );
}

#[test]
fn login_request_shape() {
    let mut req = LoginRequest {
        username: "ana".into(),
        password: "pw".into(),
        device_name: "Mac de Ana".into(),
        client_type: None,
        capabilities: None,
        scopes: None,
    };
    assert_eq!(
        serde_json::to_string(&req).unwrap(),
        r#"{"username":"ana","password":"pw","device_name":"Mac de Ana"}"#
    );
    req.client_type = Some(ClientType::App);
    req.capabilities = Some(vec!["mic".into(), "speaker".into()]);
    req.scopes = Some(vec![Scope::ConversationsWrite]);
    assert_eq!(
        serde_json::to_string(&req).unwrap(),
        r#"{"username":"ana","password":"pw","device_name":"Mac de Ana","client_type":"app","capabilities":["mic","speaker"],"scopes":["conversations:write"]}"#
    );
}

#[test]
fn login_response_decodes() {
    let r: LoginResponse = serde_json::from_str(
        r#"{"client_id":"c-7","token":"tok-1","scopes":["conversations:write","conversations:read","home:robots"],"user":{"id":"u-1","username":"ana","display_name":"Ana","role":"member"},"home":{"id":"h-1","name":"Casa"},"extra":1}"#,
    )
    .unwrap();
    assert_eq!(r.client_id, "c-7");
    assert_eq!(r.token, "tok-1");
    assert_eq!(
        r.scopes,
        vec![
            Scope::ConversationsWrite,
            Scope::ConversationsRead,
            Scope::Other
        ]
    );
    assert_eq!(r.user.username, "ana");
    assert_eq!(r.user.display_name, "Ana");
    assert_eq!(r.user.role, "member");
    assert_eq!(r.home.as_ref().map(|h| h.name.as_str()), Some("Casa"));
    // The gateway code sends `"home": null` when the device has no home row.
    let r: LoginResponse = serde_json::from_str(
        r#"{"client_id":"c-7","token":"t","scopes":[],"user":{"id":"u","username":"a","display_name":"A","role":"owner"},"home":null}"#,
    )
    .unwrap();
    assert!(r.home.is_none());
}

#[test]
fn me_decodes_and_ignores_device() {
    let m: Me = serde_json::from_str(
        r#"{"user":{"id":"u-1","username":"ana","display_name":"Ana","role":"admin"},"home":{"id":"h-1","name":"Casa"},"device":{"id":"c-7","whatever":[1,2]},"scopes":["home:admin"],"notify_client_id":null}"#,
    )
    .unwrap();
    assert_eq!(m.scopes, vec![Scope::HomeAdmin]);
    assert_eq!(m.notify_client_id, None);
    assert_eq!(m.user.role, "admin");
    let m: Me = serde_json::from_str(
        r#"{"user":{"id":"u","username":"a","display_name":"A","role":"member"},"home":null,"device":null,"scopes":[],"notify_client_id":"c-2"}"#,
    )
    .unwrap();
    assert_eq!(m.notify_client_id.as_deref(), Some("c-2"));
    assert_eq!(
        serde_json::to_string(&UpdateMe {
            notify_client_id: "c-2".into()
        })
        .unwrap(),
        r#"{"notify_client_id":"c-2"}"#
    );
}

#[test]
fn conversation_list_decodes() {
    let l: Vec<Conversation> = serde_json::from_str(
        r#"[
        {"id":"cv-1","client_id":"c-1","client_name":"Mac","agent":"main","kind":"default","day":"2026-11-26","created_at":"2026-11-26T08:00:00Z","last_turn_at":"2026-11-26T09:00:00Z","turn_count":3,"title":"Luces","owner_user_id":"u-1"},
        {"id":"cv-2","client_id":"c-1","client_name":"Mac","agent":"main","kind":"pinned","day":null,"created_at":"2026-11-26T08:00:00Z","last_turn_at":null,"turn_count":0,"title":null,"owner_user_id":null,"new_field":true}
    ]"#,
    )
    .unwrap();
    assert_eq!(l.len(), 2);
    assert_eq!(l[0].title.as_deref(), Some("Luces"));
    assert_eq!(l[0].turn_count, 3);
    assert_eq!(l[0].message_id, None);
    assert_eq!(l[1].kind, "pinned");
    assert!(l[1].day.is_none() && l[1].last_turn_at.is_none() && l[1].owner_user_id.is_none());
    // Created with a message: the reply carries its `message_id`.
    let c: Conversation = serde_json::from_str(
        r#"{"id":"cv-3","agent":"main","created_at":"2026-11-26T08:00:00Z","message_id":"m-1"}"#,
    )
    .unwrap();
    assert_eq!(c.message_id.as_deref(), Some("m-1"));
    assert_eq!((c.client_id.as_str(), c.turn_count), ("", 0));
}

#[test]
fn create_conversation_omits_absent_fields() {
    assert_eq!(
        serde_json::to_string(&CreateConversation {
            agent: None,
            message: None
        })
        .unwrap(),
        "{}"
    );
    let c = CreateConversation {
        agent: None,
        message: Some("hola".into()),
    };
    assert_eq!(serde_json::to_string(&c).unwrap(), r#"{"message":"hola"}"#);
    let dbg = format!("{c:?}");
    assert!(!dbg.contains("hola"), "{dbg}");
}

#[test]
fn agents_decodes() {
    let a: Agents = serde_json::from_str(r#"{"agents":["main","cocina"],"default":null}"#).unwrap();
    assert_eq!(a.agents, vec!["main".to_string(), "cocina".to_string()]);
    assert_eq!(a.default, None);
    let a: Agents = serde_json::from_str(r#"{"agents":[],"default":"main"}"#).unwrap();
    assert_eq!(a.default.as_deref(), Some("main"));
}

#[test]
fn history_decodes() {
    let h: History = serde_json::from_str(
        r#"{"conversation_id":"cv-1","messages":[
            {"id":"m-1","role":"user","text":"enciende la luz","timestamp":"2026-11-26T08:00:00Z"},
            {"id":"m-2","role":"assistant","text":"hecho","timestamp":null},
            {"id":"m-3","role":"system","text":"x","timestamp":null}
        ],"has_more":true}"#,
    )
    .unwrap();
    assert_eq!(h.conversation_id, "cv-1");
    assert!(h.has_more);
    assert_eq!(h.messages[0].role, Role::User);
    assert_eq!(h.messages[1].role, Role::Assistant);
    assert_eq!(h.messages[1].timestamp, None);
    assert_eq!(h.messages[2].role, Role::Other);
    let dbg = format!("{h:?}");
    assert!(!dbg.contains("enciende"), "{dbg}");
}

#[test]
fn history_message_without_id() {
    let h: History = serde_json::from_str(
        r#"{"conversation_id":"cv-1","messages":[{"role":"assistant","text":"hola","timestamp":null}],"has_more":false}"#,
    )
    .unwrap();
    assert_eq!(h.messages[0].id, None);
    assert_eq!(h.messages[0].text, "hola");
}

#[test]
fn api_error_code() {
    let e: ApiError = serde_json::from_str(r#"{"detail":"missing_scope"}"#).unwrap();
    assert_eq!(e.code(), Some("missing_scope"));
    let e: ApiError = serde_json::from_str(
        r#"{"detail":[{"type":"missing","loc":["body","username"],"msg":"Field required"}]}"#,
    )
    .unwrap();
    assert_eq!(e.code(), None);
}

#[test]
fn secrets_are_redacted() {
    let h = Handshake {
        client_key: "sekret-handshake".into(),
        protocol: PROTOCOL_VERSION,
        input_mode: InputMode::Text,
        output_mode: vec![OutputMode::Text],
        agent: None,
    };
    let p: PairResponse = serde_json::from_str(
        r#"{"client_id":"c","name":"n","token":"sekret-pair","scopes":[],"gateway":{"name":"g","version":"5"}}"#,
    )
    .unwrap();
    let req = LoginRequest {
        username: "ana".into(),
        password: "sekret-password".into(),
        device_name: "Mac".into(),
        client_type: None,
        capabilities: None,
        scopes: None,
    };
    let resp: LoginResponse = serde_json::from_str(
        r#"{"client_id":"c","token":"sekret-login","scopes":[],"user":{"id":"u","username":"a","display_name":"A","role":"member"},"home":null}"#,
    )
    .unwrap();
    let dbg = format!("{h:?} {h:#?} {p:?} {p:#?} {req:?} {req:#?} {resp:?} {resp:#?}");
    assert!(!dbg.contains("sekret"), "{dbg}");
    assert!(dbg.contains("redacted"), "{dbg}");
}
