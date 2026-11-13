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

#[test]
fn one_mib_frames_cost_at_most_3x_their_size() {
    let frames = [
        ("tool_call args [[],..]", tool_call_with_args("[],")),
        (
            "tool_call args {..}",
            tool_call_with_args(r#"{"a":[1,{}]},"#),
        ),
        ("pipeline_event many keys", pipeline_event_with_keys()),
    ];
    for (what, f) in &frames {
        assert!(f.len() <= WS_MAX_FRAME_BYTES);
        let budget = isize::try_from(3 * f.len()).unwrap();
        let before = fogar_testalloc::live_bytes();
        fogar_testalloc::reset_peak();
        let msg = decode_server(f).unwrap();
        let peak = fogar_testalloc::peak_bytes() - before;
        let retained = fogar_testalloc::live_bytes() - before;
        assert!(
            matches!(msg, ServerMsg::ToolCall(_) | ServerMsg::PipelineEvent(_)),
            "{what}"
        );
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
