//! Server -> client JSON messages (spec §5, §7, §8, §10).
//!
//! These types derive `Serialize` too (ADR-0006): the scripted fake gateway in fogar-testkit
//! produces them, and the round-trip proptests use it. Serialising a server message is never
//! done by the agent itself.

use alloc::{string::String, vec::Vec};
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned, de::IgnoredAny};
use serde_json::Value;

use crate::client::{InputMode, OutputMode};

/// Internally tagged on `"type"`. Tolerant reader: unknown types become [`ServerMsg::Unknown`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// CERTAIN
    Ready(Ready),
    /// CERTAIN
    TranscriptionPartial(Transcription),
    /// CERTAIN (final; does NOT open a turn: the client must `send` it)
    Transcription(Transcription),
    /// CERTAIN
    TurnStart(TurnStart),
    /// CERTAIN
    Token(Token),
    /// CERTAIN names; types LIKELY
    ToolCall(ToolCall),
    /// CERTAIN
    TurnEnd(TurnEnd),
    /// CERTAIN (`turn_id` since v1.19.4)
    Interrupted(Interrupted),
    /// CERTAIN
    Status(Status),
    /// CERTAIN
    Error(ErrorMsg),
    /// GUESSED beyond `stage`/`turn`
    PipelineEvent(PipelineEvent),
    /// Any future or unknown message type. Must never break the session.
    #[serde(other)]
    Unknown,
}

/// CERTAIN (JAP §1.4). May arrive AFTER one or more `Status`.
///
/// Tolerant by design (review code#5): ANY frame with `"type":"ready"` decodes as `Ready`. Every
/// field is optional; a missing, `null` or unexpectedly shaped value takes its default; unknown
/// `output_mode` values are dropped; unknown fields (e.g. an older gateway's `capabilities`) are
/// ignored. The agent only needs the fact that the session is ready.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ready {
    /// `"<client_id>:<timestamp_ms>"`: identifies the connection, not the conversation.
    #[serde(default, deserialize_with = "lenient")]
    pub session_id: String,
    /// Effective agent: trust it over the requested one. `None` if absent or `null`.
    #[serde(default, deserialize_with = "lenient")]
    pub agent: Option<String>,
    /// `None` if absent or a value this client does not know.
    #[serde(default, deserialize_with = "lenient")]
    pub input_mode: Option<InputMode>,
    /// Known modes only (unknown values are skipped).
    #[serde(default, deserialize_with = "lenient_modes")]
    pub output_mode: Vec<OutputMode>,
    #[serde(default, deserialize_with = "lenient")]
    pub requested_capabilities: Capabilities,
    #[serde(default, deserialize_with = "lenient")]
    pub live_capabilities: Capabilities,
}

/// `T` if the value has `T`'s shape, otherwise `T::default()`. The intermediate `Value` lives only
/// while one `ready` frame is decoded (once per connection).
fn lenient<'de, D: Deserializer<'de>, T: DeserializeOwned + Default>(d: D) -> Result<T, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(T::deserialize(v).unwrap_or_default())
}

fn lenient_modes<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<OutputMode>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| OutputMode::deserialize(v).ok())
            .collect(),
        _ => Vec::new(),
    })
}

/// A field whose value the agent never reads: skipped while parsing (no allocation, no
/// `serde_json::Value` tree; review sec#4). Only its presence is recorded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Skipped;

impl<'de> Deserialize<'de> for Skipped {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        IgnoredAny::deserialize(d)?;
        Ok(Skipped)
    }
}

/// CERTAIN keys. `false` is ambiguous ("not requested" or "down").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub barge_in: bool,
    #[serde(default)]
    pub tts: bool,
    #[serde(default)]
    pub transcriber: bool,
}

/// CERTAIN. A partial is the accumulated text of the current utterance; it replaces the previous one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transcription {
    pub text: String,
}

/// CERTAIN names. `turn_seq` = N & 0xFFFF (wraps; compare by equality). `turn_id` is "t-N" and
/// restarts at t-1 on every connection. No `origin` field: pushes are indistinguishable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStart {
    pub turn_id: String,
    pub turn_seq: u16,
}

/// CERTAIN
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub turn_id: String,
    pub text: String,
}

/// CERTAIN: exactly one per TurnStart (known exception JGW-286: silent push).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEnd {
    pub turn_id: String,
}

/// CERTAIN since v1.19.4; arrives after the cancelled turn's TurnEnd. Optional for old servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interrupted {
    #[serde(default)]
    pub turn_id: Option<String>,
}

/// Field names CERTAIN (JAP §1.6). `args` shape UNKNOWN and unused: skipped, not kept (sec#4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub turn_id: String,
    /// "start" | "result" (String-backed on purpose; see [`ToolCall::phase`]).
    pub phase: String,
    pub name: String,
    pub tool_call_id: String,
    /// `Some` if the frame carried `args` (the value itself is not kept).
    #[serde(default, skip_serializing)]
    pub args: Option<Skipped>,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPhase {
    Start,
    Result,
    Other,
}

impl ToolCall {
    pub fn phase(&self) -> ToolPhase {
        match self.phase.as_str() {
            "start" => ToolPhase::Start,
            "result" => ToolPhase::Result,
            _ => ToolPhase::Other,
        }
    }
}

/// CERTAIN field names. Vocabularies are open-ended, so they stay `String`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// "orchestrator" | "transcriber" | "tts"
    pub service: String,
    /// "unavailable" | "reconnecting" | "restored" | "degraded"
    pub state: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Orchestrator,
    Transcriber,
    Tts,
    Other,
}

/// `Restored` is a transition. `Degraded` = quality/capacity, not an outage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    Unavailable,
    Reconnecting,
    Restored,
    Degraded,
    Other,
}

impl Status {
    pub fn service(&self) -> Service {
        match self.service.as_str() {
            "orchestrator" => Service::Orchestrator,
            "transcriber" => Service::Transcriber,
            "tts" => Service::Tts,
            _ => Service::Other,
        }
    }
    pub fn state(&self) -> ServiceState {
        match self.state.as_str() {
            "unavailable" => ServiceState::Unavailable,
            "reconnecting" => ServiceState::Reconnecting,
            "restored" => ServiceState::Restored,
            "degraded" => ServiceState::Degraded,
            _ => ServiceState::Other,
        }
    }
}

/// CERTAIN shape (spec §10). The machine-readable reason is in `message`, not `code`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorMsg {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub fatal: bool,
    #[serde(default)]
    pub turn_id: Option<String>,
}

/// CERTAIN list for TURN_ERROR.message (JGW-228, JGW-215; JAP §1.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnErrorReason {
    OrchestratorUnavailable,
    OrchestratorSendFailed,
    OrchestratorError,
    OrchestratorDisconnected,
    TurnTimeout,
    TurnOverflow,
    TurnInProgress,
    InternalError,
    Other,
}

impl ErrorMsg {
    pub fn is_turn_error(&self) -> bool {
        self.code == "TURN_ERROR"
    }
    pub fn is_invalid_message(&self) -> bool {
        self.code == "INVALID_MESSAGE"
    }
    pub fn turn_reason(&self) -> Option<TurnErrorReason> {
        if !self.is_turn_error() {
            return None;
        }
        Some(match self.message.as_str() {
            "orchestrator_unavailable" => TurnErrorReason::OrchestratorUnavailable,
            "orchestrator_send_failed" => TurnErrorReason::OrchestratorSendFailed,
            "orchestrator_error" => TurnErrorReason::OrchestratorError,
            "orchestrator_disconnected" => TurnErrorReason::OrchestratorDisconnected,
            "turn_timeout" => TurnErrorReason::TurnTimeout,
            "turn_overflow" => TurnErrorReason::TurnOverflow,
            "turn_in_progress" => TurnErrorReason::TurnInProgress,
            "internal_error" => TurnErrorReason::InternalError,
            _ => TurnErrorReason::Other,
        })
    }
}

/// GUESSED beyond `stage` and `turn`. Only sent if `OutputMode::Status` was requested (we don't).
/// `turn` is a different counter from `turn_id`. Other fields are ignored, not kept (review sec#4:
/// a flattened map of `Value`s cost ~30x the frame size).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineEvent {
    pub stage: String,
    #[serde(default)]
    pub turn: Option<u64>,
}
