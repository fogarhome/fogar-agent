//! `GET /events` (gateway 5.0, `client-protocol.md` §16 at 290efa0): the SSE reader and the event
//! vocabulary, plus the REST bodies of §16 (conversation state, post a message).

use fogar_proto::conversations::{Accepted, ConversationState, PostMessage};
use fogar_proto::events::{Event, MsgState, decode_event};
use fogar_proto::server::{TurnEndReason, TurnOrigin};
use fogar_proto::sse::{MAX_EVENT_BYTES, SseItem, SseReader};
use std::borrow::Cow;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Event { id: Option<String>, data: String },
    Comment,
    Oversize,
}

fn feed_all(r: &mut SseReader, chunk: &[u8], out: &mut Vec<Item>) {
    r.feed(chunk, &mut |it| {
        out.push(match it {
            SseItem::Event { id, data } => Item::Event {
                id: id.map(String::from),
                data: String::from(data),
            },
            SseItem::Comment => Item::Comment,
            SseItem::Oversize => Item::Oversize,
        })
    });
}

fn items(chunks: &[&[u8]]) -> Vec<Item> {
    let mut r = SseReader::new();
    let mut out = Vec::new();
    for c in chunks {
        feed_all(&mut r, c, &mut out);
    }
    out
}

fn ev(id: Option<&str>, data: &str) -> Item {
    Item::Event {
        id: id.map(String::from),
        data: String::from(data),
    }
}

/// §16, verbatim (with the doc's `…` placeholders, a 3-byte UTF-8 character).
const DOC_STREAM: &str = "retry: 1000\n\
\n\
id: 1795689600000001\n\
data: {\"type\":\"ready\",\"protocol\":5,\"device_id\":\"…\"}\n\
\n\
id: 1795689600000002\n\
data: {\"type\":\"message\",\"id\":\"run-1\",\"conversation\":\"c1\",\"text\":\"hola\",\"from\":{\"source\":\"device\",\"device_id\":\"…\",\"name\":\"Mac\",\"self\":true},\"state\":\"pending\"}\n\
\n\
id: 1795689600000003\n\
data: {\"type\":\"turn_start\",\"turn_id\":\"9f3c…\",\"conversation\":\"c1\",\"origin\":\"user\",\"by\":{\"device_id\":\"…\",\"name\":\"Mac\",\"self\":true}}\n\
\n";

#[test]
fn doc_example_stream_decodes() {
    let got = items(&[DOC_STREAM.as_bytes()]);
    assert_eq!(got.len(), 3, "{got:?}");
    let ids: Vec<_> = got
        .iter()
        .map(|i| match i {
            Item::Event { id, .. } => id.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        ids,
        [
            Some("1795689600000001".into()),
            Some("1795689600000002".into()),
            Some("1795689600000003".into())
        ]
    );
    let evs: Vec<Event> = got
        .iter()
        .map(|i| match i {
            Item::Event { data, .. } => decode_event(data),
            _ => unreachable!(),
        })
        .collect();
    match &evs[0] {
        Event::Ready {
            protocol,
            device_id,
        } => {
            assert_eq!(*protocol, Some(5));
            assert_eq!(device_id.as_deref(), Some("…"));
        }
        other => panic!("{other:?}"),
    }
    match &evs[1] {
        Event::Message(m) => {
            assert_eq!(m.id, "run-1");
            assert_eq!(m.conversation.as_deref(), Some("c1"));
            assert_eq!(m.text, "hola");
            assert_eq!(m.from.source, "device");
            assert_eq!(m.from.device_id.as_deref(), Some("…"));
            assert_eq!(m.from.name.as_deref(), Some("Mac"));
            assert!(m.from.self_);
            assert_eq!(m.state, MsgState::Pending);
        }
        other => panic!("{other:?}"),
    }
    match &evs[2] {
        Event::TurnStart(t) => {
            assert_eq!(t.turn_id, "9f3c…");
            assert_eq!(t.conversation.as_deref(), Some("c1"));
            assert_eq!(t.origin, Some(TurnOrigin::User));
            let by = t.by.as_ref().unwrap();
            assert_eq!(
                (by.device_id.as_str(), by.name.as_str(), by.self_),
                ("…", "Mac", true)
            );
            assert_eq!(t.text, None);
        }
        other => panic!("{other:?}"),
    }
}

/// Every SSE rule the reader honours, in one stream: a leading BOM, LF / CR / CRLF line ends,
/// `retry:`/`event:`/unknown fields ignored, several `data:` lines joined with `\n`, a bare
/// `data` (no colon), `data:` with no space, an `id` holding NUL ignored, an event with no `data`
/// never dispatched, a comment, an event cut by the end of the stream never dispatched.
const EDGE_STREAM: &[u8] = b"\xEF\xBB\xBFretry: 1000\r\n\r\n\
: ping\n\n\
id: 7\revent: x\rdata: a\rdata:b\rdata\rfoo: bar\r\r\
id: 8\r\nid: \x00bad\r\ndata:  two spaces \xE2\x80\xA6\r\n\r\n\
id: 9\n\n\
data: no id\n\n\
data: cut at EOF";

fn edge_expected() -> Vec<Item> {
    vec![
        Item::Comment,
        ev(Some("7"), "a\nb\n"),
        ev(Some("8"), " two spaces …"),
        ev(None, "no id"),
    ]
}

#[test]
fn any_chunking_gives_the_same_items() {
    assert_eq!(items(&[EDGE_STREAM]), edge_expected());
    for at in 0..=EDGE_STREAM.len() {
        let (a, b) = EDGE_STREAM.split_at(at);
        assert_eq!(items(&[a, b]), edge_expected(), "split at {at}");
    }
    let bytes: Vec<&[u8]> = EDGE_STREAM.chunks(1).collect();
    assert_eq!(items(&bytes), edge_expected(), "byte by byte");
    // The §16 sample too (its `…` is split mid-character at some offsets).
    let doc = DOC_STREAM.as_bytes();
    let whole = items(&[doc]);
    for at in 0..=doc.len() {
        let (a, b) = doc.split_at(at);
        assert_eq!(items(&[a, b]), whole, "split at {at}");
    }
    // A CR at the end of one chunk and its LF at the start of the next is ONE line end.
    assert_eq!(
        items(&[b"data: x\r", b"\n", b"\r", b"\ndata: y\n\n"]),
        vec![ev(None, "x"), ev(None, "y")]
    );
    // A BOM only counts at the very start of the stream.
    assert_eq!(
        items(&[b"\xEF", b"\xBB\xBFdata: x\n\n\xEF\xBB\xBFdata: y\n\n"]),
        vec![ev(None, "x")]
    );
}

#[test]
fn oversize_event_is_skipped_and_next_parses() {
    let mut stream = Vec::new();
    stream.extend_from_slice(b"id: 1\ndata: ");
    stream.extend(std::iter::repeat_n(b'x', MAX_EVENT_BYTES));
    stream.extend_from_slice(b"\ndata: more\n\nid: 2\ndata: {\"type\":\"resync\"}\n\n");
    // One big line, and the same size spread over many short `data:` lines.
    let mut many = Vec::new();
    many.extend_from_slice(b"id: 3\n");
    for _ in 0..MAX_EVENT_BYTES / 2 {
        many.extend_from_slice(b"data: xx\n");
    }
    many.extend_from_slice(b"\nid: 4\ndata: ok\n\n");
    for (s, next) in [
        (&stream, ev(Some("2"), "{\"type\":\"resync\"}")),
        (&many, ev(Some("4"), "ok")),
    ] {
        let chunks: Vec<&[u8]> = s.chunks(1000).collect();
        assert_eq!(items(&chunks), vec![Item::Oversize, next.clone()]);
        assert_eq!(items(&[s]), vec![Item::Oversize, next]);
    }
    // Just under the cap still parses.
    let mut fits = b"data: ".to_vec();
    fits.extend(std::iter::repeat_n(b'y', MAX_EVENT_BYTES - 64));
    fits.extend_from_slice(b"\n\n");
    let got = items(&[&fits]);
    assert!(matches!(&got[..], [Item::Event { data, .. }] if data.len() == MAX_EVENT_BYTES - 64));
}

#[test]
fn comment_is_comment() {
    assert_eq!(items(&[b": ping\n\n"]), vec![Item::Comment]);
    assert_eq!(items(&[b":\n"]), vec![Item::Comment]);
}

#[test]
fn unknown_type_is_unknown() {
    assert_eq!(
        decode_event(r#"{"type":"shiny_new","x":1}"#),
        Event::Unknown
    );
    assert_eq!(decode_event(r#"{"no_type":1}"#), Event::Unknown);
    assert_eq!(decode_event("not json"), Event::Unknown);
    assert_eq!(decode_event(r#"["type","ready"]"#), Event::Unknown);
    assert_eq!(decode_event(r#"{"type":"resync"}"#), Event::Resync);
    // Push audio is recognised and its payload never decoded.
    assert_eq!(
        decode_event(r#"{"type":"audio","turn_id":"t","conversation":"c1","data":"AAAA"}"#),
        Event::Audio
    );
}

#[test]
fn unknown_field_is_dropped() {
    let e = decode_event(
        r#"{"type":"token","turn_id":"t","conversation":"c1","text":"hi","new":{"a":[1,2]}}"#,
    );
    match e {
        Event::Token(t) => {
            assert_eq!((t.turn_id.as_str(), t.text.as_str()), ("t", "hi"));
            assert_eq!(t.conversation.as_deref(), Some("c1"));
        }
        other => panic!("{other:?}"),
    }
    // A known type with oddly typed fields still decodes (lenient): never lost as `Unknown`.
    let e = decode_event(
        r#"{"type":"message","id":"m","conversation":7,"text":"x","from":"nope","state":7}"#,
    );
    match e {
        Event::Message(m) => {
            assert_eq!(m.id, "m");
            assert_eq!(m.conversation, None);
            assert_eq!(m.from.name, None);
            assert!(!m.from.self_);
            assert_eq!(m.state, MsgState::Other);
        }
        other => panic!("{other:?}"),
    }
    let e = decode_event(
        r#"{"type":"message","id":"m","text":"x","from":{"source":"ha","name":null,"self":"x"},"state":"withdrawn"}"#,
    );
    match e {
        Event::Message(m) => {
            assert_eq!(m.from.source, "ha");
            assert_eq!(m.from.name, None);
            assert!(!m.from.self_);
            assert_eq!(m.state, MsgState::Withdrawn);
        }
        other => panic!("{other:?}"),
    }
    // A push: `by` is null.
    match decode_event(
        r#"{"type":"turn_start","turn_id":"t","conversation":null,"origin":"push","by":null}"#,
    ) {
        Event::TurnStart(t) => {
            assert_eq!(t.by, None);
            assert_eq!(t.conversation, None);
            assert_eq!(t.origin, Some(TurnOrigin::Push));
        }
        other => panic!("{other:?}"),
    }
    match decode_event(r#"{"type":"turn_end","turn_id":"t","conversation":"c1","reason":"new"}"#) {
        Event::TurnEnd(t) => {
            assert_eq!(t.reason, Some(TurnEndReason::Error));
            assert_eq!(t.conversation.as_deref(), Some("c1"));
        }
        other => panic!("{other:?}"),
    }
    match decode_event(
        r#"{"type":"error","code":"TURN_ERROR","message":"orchestrator_error","fatal":false,"turn_id":"t","conversation":"c1"}"#,
    ) {
        Event::Error(e) => {
            assert_eq!(e.code, "TURN_ERROR");
            assert_eq!(e.message, "orchestrator_error");
            assert_eq!(e.turn_id.as_deref(), Some("t"));
            assert_eq!(e.conversation.as_deref(), Some("c1"));
        }
        other => panic!("{other:?}"),
    }
    match decode_event(
        r#"{"type":"turn_progress","turn_id":"t","phase":"tool","tool":"ha_call_service","conversation":"c1"}"#,
    ) {
        Event::TurnProgress(p) => {
            assert_eq!(p.tool.as_deref(), Some("ha_call_service"));
            assert_eq!(p.conversation.as_deref(), Some("c1"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        decode_event(
            r#"{"type":"tool_call","turn_id":"t","args":{"secret":1},"conversation":"c1"}"#
        ),
        Event::ToolCall
    );
    assert_eq!(
        decode_event(r#"{"type":"ready","protocol":"5"}"#),
        Event::Ready {
            protocol: None,
            device_id: None
        }
    );
}

#[test]
fn event_debug_hides_text() {
    let evs = [
        decode_event(
            r#"{"type":"message","id":"m","text":"secreto1","from":{"source":"device","device_id":"d","name":"Ana secreta","self":false},"state":"delivered"}"#,
        ),
        decode_event(
            r#"{"type":"turn_start","turn_id":"t","origin":"user","by":{"device_id":"d","name":"Ana secreta","self":false}}"#,
        ),
        decode_event(r#"{"type":"token","turn_id":"t","text":"secreto1"}"#),
    ];
    for e in &evs {
        let dbg = format!("{e:?}");
        assert!(!dbg.contains("secreto") && !dbg.contains("Ana"), "{dbg}");
    }
    let s: ConversationState = serde_json::from_str(
        r#"{"id":"c1","agent":"main","created_at":"x","title":"secreto","turn":{"type":"turn_start","turn_id":"t","text":"secreto"},"pending":[],"as_of":1}"#,
    )
    .unwrap();
    let dbg = format!("{s:?}");
    assert!(!dbg.contains("secreto"), "{dbg}");
}

#[test]
fn conversation_state_decodes() {
    // §16's example with the list fields filled in (the doc elides them as `"...": "..."`;
    // the gateway sends `base.model_dump()`, i.e. a list item). Its `pending` item is as sparse
    // as in the doc.
    let s: ConversationState = serde_json::from_str(
        r#"{"id":"c1","client_id":"c-1","client_name":"Mac","agent":"main","kind":"created","day":null,"created_at":"2026-11-26T08:00:00Z","last_turn_at":null,"turn_count":2,"title":null,"owner_user_id":"u-1",
            "turn":{"type":"turn_start","turn_id":"9f3c…","conversation":"c1","origin":"push","by":null,"text":"Lo que lleva"},
            "pending":[{"type":"message","id":"run-2","state":"pending"}],
            "as_of":1795689600000003}"#,
    )
    .unwrap();
    assert_eq!(s.conversation.id, "c1");
    assert_eq!(s.conversation.agent, "main");
    assert_eq!(s.as_of, 1_795_689_600_000_003);
    let t = s.turn.unwrap();
    assert_eq!(t.turn_id, "9f3c…");
    assert_eq!(t.origin, Some(TurnOrigin::Push));
    assert_eq!(t.by, None);
    assert_eq!(t.text.as_deref(), Some("Lo que lleva"));
    assert_eq!(s.pending.len(), 1);
    assert_eq!(s.pending[0].id, "run-2");
    assert_eq!(s.pending[0].state, MsgState::Pending);
    // No turn, nothing pending.
    let s: ConversationState = serde_json::from_str(
        r#"{"id":"c2","agent":"main","created_at":"x","turn":null,"pending":[],"as_of":5}"#,
    )
    .unwrap();
    assert!(s.turn.is_none() && s.pending.is_empty());
    assert_eq!(s.as_of, 5);
    // `as_of` is what makes the state usable: without it, it is an error.
    assert!(
        serde_json::from_str::<ConversationState>(
            r#"{"id":"c2","agent":"main","created_at":"x","turn":null,"pending":[]}"#
        )
        .is_err()
    );
}

#[test]
fn post_message_shape() {
    let p = PostMessage {
        text: Cow::Borrowed("hola"),
        interrupt: None,
    };
    assert_eq!(serde_json::to_string(&p).unwrap(), r#"{"text":"hola"}"#);
    assert!(!format!("{p:?}").contains("hola"));
    let p = PostMessage {
        text: Cow::Borrowed("ya"),
        interrupt: Some(true),
    };
    assert_eq!(
        serde_json::to_string(&p).unwrap(),
        r#"{"text":"ya","interrupt":true}"#
    );
    let a: Accepted = serde_json::from_str(r#"{"message_id":"run-9","x":1}"#).unwrap();
    assert_eq!(a.message_id, "run-9");
}
