//! The events of `GET /events` (gateway 5.0, `client-protocol.md` §16 and §5 at 290efa0; JSON
//! exactly as built by `services/events.py`). Each SSE `data` is one JSON object: the WebSocket's
//! vocabulary plus `conversation` on every event.
//!
//! Tolerant reader: every field is optional and an oddly typed value takes its default, so a known
//! `type` never degrades to [`Event::Unknown`]; only text that is not a JSON object with a string
//! `type` does. `Debug` never shows message text, reply text or a sender's name.

use alloc::string::String;
use core::fmt;
use core::marker::PhantomData;
use serde::{
    Deserialize, Deserializer,
    de::{self, MapAccess, SeqAccess, Visitor, value::MapAccessDeserializer},
};

use crate::codec::Tag;
use crate::server::{
    FromJsonStr, Lenient, ProgressPhase, Redacted, TurnEndReason, TurnOrigin, drain_seq,
    lenient_bool, lenient_end_reason, lenient_opt_string, lenient_origin, lenient_other,
    lenient_phase, lenient_string, lenient_tool, lenient_u32, working,
};

/// One `/events` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The first event of every stream. On resume it carries no SSE id.
    Ready {
        protocol: Option<u32>,
        device_id: Option<String>,
    },
    /// The device's buffer no longer holds the `Last-Event-ID`: re-fetch what is open.
    Resync,
    Message(MessageEv),
    TurnStart(TurnStartEv),
    Token(TokenEv),
    TurnProgress(TurnProgressEv),
    /// Only with `tool_calls_enabled`; payload (arguments, results) never decoded.
    ToolCall,
    Error(ErrorEv),
    TurnEnd(TurnEndEv),
    /// Push audio (base64 PCM, `/events?audio=1` or `speak`): payload never decoded.
    Audio,
    Unknown,
}

/// Decodes one event's `data`. Two passes like [`crate::decode_server`]: the `type` first, then
/// only the known shapes; `audio` and `tool_call` are never parsed past their tag.
pub fn decode_event(data: &str) -> Event {
    fn body<'a, T: Deserialize<'a>>(data: &'a str) -> Option<T> {
        serde_json::from_str(data).ok()
    }
    let Some(tag) = body::<Tag<'_>>(data) else {
        return Event::Unknown;
    };
    let ev = match tag.ty.as_ref() {
        "ready" => body::<ReadyEv>(data).map(|r| Event::Ready {
            protocol: r.protocol,
            device_id: r.device_id,
        }),
        "resync" => Some(Event::Resync),
        "message" => body(data).map(Event::Message),
        "turn_start" => body(data).map(Event::TurnStart),
        "token" => body(data).map(Event::Token),
        "turn_progress" => body(data).map(Event::TurnProgress),
        "tool_call" => Some(Event::ToolCall),
        "error" => body(data).map(Event::Error),
        "turn_end" => body(data).map(Event::TurnEnd),
        "audio" => Some(Event::Audio),
        _ => None,
    };
    ev.unwrap_or(Event::Unknown)
}

#[derive(Deserialize)]
struct ReadyEv {
    #[serde(default, deserialize_with = "lenient_u32")]
    protocol: Option<u32>,
    #[serde(default, deserialize_with = "lenient_opt_string")]
    device_id: Option<String>,
}

/// A user message of a conversation and its state; sent again on every state change. `id` is the
/// OpenClaw run id (the `message_id` of `POST …/messages`), NOT the id of the same message in
/// `GET …/messages` history.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct MessageEv {
    #[serde(default, deserialize_with = "lenient_string")]
    pub id: String,
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub conversation: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub text: String,
    #[serde(default, deserialize_with = "lenient_from")]
    pub from: MessageFrom,
    #[serde(default = "msg_other", deserialize_with = "lenient_state")]
    pub state: MsgState,
}

impl fmt::Debug for MessageEv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageEv")
            .field("id", &self.id)
            .field("conversation", &self.conversation)
            .field("text", &Redacted(self.text.len()))
            .field("from", &self.from)
            .field("state", &self.state)
            .finish()
    }
}

/// `message.from`. `source` is `"device"` today; any other value is a generic origin.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
pub struct MessageFrom {
    #[serde(default, deserialize_with = "lenient_string")]
    pub source: String,
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub device_id: Option<String>,
    /// The sending device's name (`null` when the gateway has none).
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub name: Option<String>,
    /// Sent by this very device.
    #[serde(default, rename = "self", deserialize_with = "lenient_bool")]
    pub self_: bool,
}

impl fmt::Debug for MessageFrom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageFrom")
            .field("source", &self.source)
            .field("device_id", &self.device_id)
            .field("name", &self.name.as_ref().map(|n| Redacted(n.len())))
            .field("self_", &self.self_)
            .finish()
    }
}

/// `message.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MsgState {
    /// Waiting in OpenClaw's queue behind a running turn.
    Pending,
    /// Reached the agent (its turn starts, or it joined the running one).
    Delivered,
    /// Withdrawn, or stopped before it started.
    Withdrawn,
    /// Missing or a value this client does not know.
    Other,
}

impl FromJsonStr for MsgState {
    fn from_json_str<E: de::Error>(s: &str) -> Option<Self> {
        Some(
            MsgState::deserialize(de::value::StrDeserializer::<E>::new(s))
                .unwrap_or(MsgState::Other),
        )
    }
}

fn msg_other() -> MsgState {
    MsgState::Other
}

fn lenient_state<'de, D: Deserializer<'de>>(d: D) -> Result<MsgState, D::Error> {
    Ok(d.deserialize_any(Lenient::<MsgState>(PhantomData))?
        .unwrap_or(MsgState::Other))
}

/// `turn_start` (no `turn_seq` over SSE). `turn_id` is the conversation's turn id: the same on
/// every device and across reconnects. `text` is only present in the `turn` of
/// `GET /conversations/{id}` (what the running turn has said so far); over `/events` a mid-turn
/// join gets it as a `token` instead.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct TurnStartEv {
    #[serde(default, deserialize_with = "lenient_string")]
    pub turn_id: String,
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub conversation: Option<String>,
    #[serde(default, deserialize_with = "lenient_origin")]
    pub origin: Option<TurnOrigin>,
    /// Who started it; `None` for a push.
    #[serde(default, deserialize_with = "lenient_obj")]
    pub by: Option<By>,
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub text: Option<String>,
}

impl fmt::Debug for TurnStartEv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnStartEv")
            .field("turn_id", &self.turn_id)
            .field("conversation", &self.conversation)
            .field("origin", &self.origin)
            .field("by", &self.by)
            .field("text", &self.text.as_ref().map(|t| Redacted(t.len())))
            .finish()
    }
}

/// `turn_start.by`.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct By {
    #[serde(default, deserialize_with = "lenient_string")]
    pub device_id: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub name: String,
    #[serde(default, rename = "self", deserialize_with = "lenient_bool")]
    pub self_: bool,
}

impl fmt::Debug for By {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("By")
            .field("device_id", &self.device_id)
            .field("name", &Redacted(self.name.len()))
            .field("self_", &self.self_)
            .finish()
    }
}

/// Reply text in streaming order; concatenate per `turn_id`.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct TokenEv {
    #[serde(default, deserialize_with = "lenient_string")]
    pub turn_id: String,
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub conversation: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub text: String,
}

impl fmt::Debug for TokenEv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenEv")
            .field("turn_id", &self.turn_id)
            .field("conversation", &self.conversation)
            .field("text", &Redacted(self.text.len()))
            .finish()
    }
}

/// What the turn is doing now (§5 `turn_progress`); a `token` makes it stale.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TurnProgressEv {
    #[serde(default, deserialize_with = "lenient_string")]
    pub turn_id: String,
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub conversation: Option<String>,
    #[serde(default = "working", deserialize_with = "lenient_phase")]
    pub phase: ProgressPhase,
    #[serde(default, deserialize_with = "lenient_tool")]
    pub tool: Option<String>,
}

/// Over `/events` always `code: "TURN_ERROR"` with the reason in `message` (`events.py`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ErrorEv {
    #[serde(default, deserialize_with = "lenient_string")]
    pub code: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub message: String,
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub turn_id: Option<String>,
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub conversation: Option<String>,
}

/// Exactly one per `turn_start`. `reason`: an unknown value decodes as `Error` (§5: «treat any
/// other value as error»); `None` only when absent or not a string.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TurnEndEv {
    #[serde(default, deserialize_with = "lenient_string")]
    pub turn_id: String,
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub conversation: Option<String>,
    #[serde(default, deserialize_with = "lenient_end_reason")]
    pub reason: Option<TurnEndReason>,
}

// --- a nested object that may be `null` or the wrong shape -----------------------------------

/// `Some(T)` for a JSON object (its fields as lenient as `T` makes them), `None` for anything else.
struct LenientObj<T>(PhantomData<T>);

impl<'de, T: Deserialize<'de>> Visitor<'de> for LenientObj<T> {
    type Value = Option<T>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_map<A: MapAccess<'de>>(self, a: A) -> Result<Self::Value, A::Error> {
        T::deserialize(MapAccessDeserializer::new(a)).map(Some)
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, a: A) -> Result<Self::Value, A::Error> {
        drain_seq(a)?;
        Ok(None)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(None)
    }
    lenient_other!(None);
}

pub(crate) fn lenient_obj<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    d.deserialize_any(LenientObj(PhantomData))
}

fn lenient_from<'de, D: Deserializer<'de>>(d: D) -> Result<MessageFrom, D::Error> {
    Ok(lenient_obj(d)?.unwrap_or_default())
}
