//! Review sec#4: a decoded server frame must not cost much more memory than its bytes, neither
//! while decoding (peak) nor while it waits in the inbound queue (retained). One test per binary:
//! the counting allocator is process-global.
#![allow(clippy::unwrap_used)]

#[global_allocator]
static A: fogar_testalloc::Counting = fogar_testalloc::Counting;

use fogar_proto::{ServerMsg, decode_server, limits::WS_MAX_FRAME_BYTES};

fn tool_call_with_args(fill: &str) -> String {
    let mut s = String::from(
        r#"{"type":"tool_call","turn_id":"t","phase":"start","name":"n","tool_call_id":"i","args":["#,
    );
    while s.len() + fill.len() + 4 < WS_MAX_FRAME_BYTES {
        s.push_str(fill);
    }
    s.push_str("0]}");
    s
}

fn pipeline_event_with_keys() -> String {
    let mut p = String::from(r#"{"type":"pipeline_event","stage":"s""#);
    let mut i = 0u32;
    while p.len() + 16 < WS_MAX_FRAME_BYTES {
        p.push_str(&format!(r#","{i:x}":0"#));
        i += 1;
    }
    p.push('}');
    p
}

/// `{"type":"<ty>",<prefix>"<field>":[<fill>,<fill>,...,0]<suffix>}` filled to the frame limit.
fn frame_with_array(ty: &str, prefix: &str, field: &str, fill: &str, suffix: &str) -> String {
    let mut s = format!(r#"{{"type":"{ty}",{prefix}"{field}":["#);
    while s.len() + fill.len() + suffix.len() + 4 < WS_MAX_FRAME_BYTES {
        s.push_str(fill);
    }
    s.push_str("0]");
    s.push_str(suffix);
    s.push('}');
    s
}

/// `{"type":"<ty>",<prefix>"<field>":"xxxx..."<suffix>}` filled to the frame limit.
fn frame_with_string(ty: &str, prefix: &str, field: &str, suffix: &str) -> String {
    let head = format!(r#"{{"type":"{ty}",{prefix}"{field}":""#);
    let n = WS_MAX_FRAME_BYTES - head.len() - suffix.len() - 2;
    format!("{head}{}\"{suffix}}}", "x".repeat(n))
}

#[test]
fn one_mib_frames_cost_at_most_3x_their_size() {
    let frames = [
        ("tool_call args [[],..]", tool_call_with_args("[],")),
        (
            "tool_call args {..}",
            tool_call_with_args(r#"{"a":[1,{}]},"#),
        ),
        ("pipeline_event many keys", pipeline_event_with_keys()),
        // Round-2 review F5: `ready` went through serde_json::Value (16x for 1 MiB).
        (
            "ready requested_capabilities [[],..]",
            frame_with_array("ready", "", "requested_capabilities", "[],", ""),
        ),
        (
            "ready live_capabilities [{..},..]",
            frame_with_array("ready", "", "live_capabilities", r#"{"a":[1,{}]},"#, ""),
        ),
        (
            "ready output_mode [{..},..]",
            frame_with_array("ready", "", "output_mode", r#"{"a":[1,{}]},"#, ""),
        ),
        (
            "ready session_id [[],..]",
            frame_with_array("ready", "", "session_id", "[],", ""),
        ),
        (
            "ready agent [{..},..]",
            frame_with_array("ready", "", "agent", r#"{"a":[1,{}]},"#, ""),
        ),
        (
            "ready input_mode [[],..]",
            frame_with_array("ready", "", "input_mode", "[],", ""),
        ),
        (
            "ready unknown field",
            frame_with_array("ready", r#""session_id":"c:1","#, "zzz", "[],", ""),
        ),
        (
            "ready session_id 1 MiB string",
            frame_with_string("ready", "", "session_id", ""),
        ),
        // Every other frame type: a huge unknown field, and a huge legitimate string.
        (
            "transcription_partial unknown field",
            frame_with_array("transcription_partial", r#""text":"a","#, "zzz", "[],", ""),
        ),
        (
            "transcription text",
            frame_with_string("transcription", "", "text", ""),
        ),
        (
            "turn_start unknown field",
            frame_with_array(
                "turn_start",
                r#""turn_id":"t-1","turn_seq":1,"#,
                "zzz",
                "[],",
                "",
            ),
        ),
        (
            "token text",
            frame_with_string("token", r#""turn_id":"t-1","#, "text", ""),
        ),
        (
            "tool_call result",
            frame_with_string(
                "tool_call",
                r#""turn_id":"t","phase":"result","name":"n","tool_call_id":"i","#,
                "result",
                "",
            ),
        ),
        (
            "turn_end unknown field",
            frame_with_array(
                "turn_end",
                r#""turn_id":"t-1","#,
                "zzz",
                r#"{"a":[1,{}]},"#,
                "",
            ),
        ),
        (
            "interrupted unknown field",
            frame_with_array("interrupted", "", "zzz", "[],", ""),
        ),
        (
            "status message",
            frame_with_string(
                "status",
                r#""service":"tts","state":"degraded","#,
                "message",
                "",
            ),
        ),
        (
            "error message",
            frame_with_string("error", r#""code":"TURN_ERROR","#, "message", ""),
        ),
        (
            "unknown type",
            frame_with_array("from_the_future", "", "payload", r#"{"a":[1,{}]},"#, ""),
        ),
    ];
    for (what, f) in &frames {
        assert!(f.len() <= WS_MAX_FRAME_BYTES);
        let budget = isize::try_from(3 * f.len()).unwrap();
        let before = fogar_testalloc::live_bytes();
        fogar_testalloc::reset_peak();
        let msg = decode_server(f).unwrap_or_else(|e| panic!("{what}: {e:?}"));
        let peak = fogar_testalloc::peak_bytes() - before;
        let retained = fogar_testalloc::live_bytes() - before;
        // The frame decodes (tolerantly) as its own type, never as an error.
        let ty = f.split('"').nth(3).unwrap();
        let ok = match &msg {
            ServerMsg::Ready(_) => ty == "ready",
            ServerMsg::TranscriptionPartial(_) => ty == "transcription_partial",
            ServerMsg::Transcription(_) => ty == "transcription",
            ServerMsg::TurnStart(_) => ty == "turn_start",
            ServerMsg::Token(_) => ty == "token",
            ServerMsg::ToolCall(_) => ty == "tool_call",
            ServerMsg::TurnEnd(_) => ty == "turn_end",
            ServerMsg::Interrupted(_) => ty == "interrupted",
            ServerMsg::Status(_) => ty == "status",
            ServerMsg::Error(_) => ty == "error",
            ServerMsg::PipelineEvent(_) => ty == "pipeline_event",
            ServerMsg::Unknown => ty == "from_the_future",
        };
        assert!(ok, "{what}: decoded as {msg:?}");
        assert!(
            peak <= budget,
            "{what}: decoding a {} B frame peaked at {peak} B",
            f.len()
        );
        assert!(
            retained <= budget,
            "{what}: the decoded message retains {retained} B for a {} B frame",
            f.len()
        );
        drop(msg);
    }
}
