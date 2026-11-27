//! Conversation REST bodies: list, create, agents, history.
//! CERTAIN (5.0, M22): gateway README «API» table, `client-protocol.md` §16 and
//! `models/admin_schemas.py` at 290efa0. Timestamps stay `String` (no date crate in `no_std`).

use alloc::{borrow::Cow, string::String, vec::Vec};
use core::fmt;
use serde::{Deserialize, Serialize};

use crate::events::{MessageEv, TurnStartEv};

/// One item of `GET /conversations`, or the `201` of `POST /conversations`. `Debug` hides
/// `title` (OpenClaw derives it from the user's text) and `client_name` (the person named
/// the device).
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct Conversation {
    pub id: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_name: String,
    pub agent: String,
    /// `default` | `created`; may grow.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub day: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub last_turn_at: Option<String>,
    #[serde(default)]
    pub turn_count: u32,
    #[serde(default)]
    pub title: Option<String>,
    /// `None`: the home's; else private to that user.
    #[serde(default)]
    pub owner_user_id: Option<String>,
    /// Only when created with `message`.
    #[serde(default)]
    pub message_id: Option<String>,
}

impl fmt::Debug for Conversation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conversation")
            .field("id", &self.id)
            .field("client_id", &self.client_id)
            .field("client_name_bytes", &self.client_name.len())
            .field("agent", &self.agent)
            .field("kind", &self.kind)
            .field("day", &self.day)
            .field("created_at", &self.created_at)
            .field("last_turn_at", &self.last_turn_at)
            .field("turn_count", &self.turn_count)
            .field("title", &self.title.as_ref().map(|_| "<redacted>"))
            .field("owner_user_id", &self.owner_user_id)
            .field("message_id", &self.message_id)
            .finish()
    }
}

/// `POST /conversations` (the gateway forbids extra fields). Absent fields are not sent.
/// `Debug` hides `message`: user text, never logged.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct CreateConversation<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Cow<'a, str>>,
}

impl fmt::Debug for CreateConversation<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreateConversation")
            .field("agent", &self.agent)
            .field("message", &self.message.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// `GET /conversations/agents`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Agents {
    pub agents: Vec<String>,
    #[serde(default)]
    pub default: Option<String>,
}

/// `GET /conversations/{id}/messages`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct History {
    pub conversation_id: String,
    pub messages: Vec<HistoryMessage>,
    pub has_more: bool,
}

/// `Debug` hides `text`: user content.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct HistoryMessage {
    /// OpenClaw's id; absent when OpenClaw did not give one.
    #[serde(default)]
    pub id: Option<String>,
    pub role: Role,
    pub text: String,
    #[serde(default)]
    pub timestamp: Option<String>,
}

impl fmt::Debug for HistoryMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HistoryMessage")
            .field("id", &self.id)
            .field("role", &self.role)
            .field("text", &"<redacted>")
            .field("timestamp", &self.timestamp)
            .finish()
    }
}

/// `GET /conversations/{id}` (§16): the list item plus its live state. Drop this conversation's
/// `/events` events with an SSE id `<= as_of`. The flattened part is buffered by serde while
/// parsing: callers cap the body size before decoding.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ConversationState {
    #[serde(flatten)]
    pub conversation: Conversation,
    /// The running turn with the text it has so far, or `None`.
    #[serde(default, deserialize_with = "crate::events::lenient_obj")]
    pub turn: Option<TurnSnapshot>,
    /// Its pending messages.
    #[serde(default)]
    pub pending: Vec<MessageEv>,
    /// The id of the last `/events` event this state includes.
    pub as_of: u64,
}

/// The `turn` of [`ConversationState`]: a `turn_start` plus the text so far.
pub type TurnSnapshot = TurnStartEv;

/// `POST /conversations/{id}/messages`. Absent fields are not sent (no `speak` until voice).
/// `Debug` hides `text`.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct PostMessage<'a> {
    pub text: Cow<'a, str>,
    /// `true`: «send now» (cuts the running turn).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interrupt: Option<bool>,
}

impl fmt::Debug for PostMessage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostMessage")
            .field("text", &"<redacted>")
            .field("interrupt", &self.interrupt)
            .finish()
    }
}

/// `202` of `POST …/messages`: the id its `message` event carries.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Accepted {
    pub message_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    #[serde(other)]
    Other,
}
