//! Server -> client JSON messages (spec §5, §7, §8, §10).
//!
//! These types derive `Serialize` too (ADR-0006): the scripted fake gateway in fogar-testkit
//! produces them, and the round-trip proptests use it. Serialising a server message is never
//! done by the agent itself.

use alloc::{string::String, vec::Vec};
use core::fmt;
use core::marker::PhantomData;
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor, value::StrDeserializer},
};

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
    /// CERTAIN (3.0.1 §5, #254). Only with `status` in `output_mode`. The turn's current state;
    /// a `token` makes it stale.
    TurnProgress(TurnProgress),
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
    #[serde(default, deserialize_with = "lenient_string")]
    pub session_id: String,
    /// Effective agent: trust it over the requested one. `None` if absent or `null`.
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub agent: Option<String>,
    /// `None` if absent or a value this client does not know.
    #[serde(default, deserialize_with = "lenient_input_mode")]
    pub input_mode: Option<InputMode>,
    /// Known modes only (unknown values are skipped).
    #[serde(default, deserialize_with = "lenient_modes")]
    pub output_mode: Vec<OutputMode>,
    #[serde(default, deserialize_with = "lenient_capabilities")]
    pub requested_capabilities: Capabilities,
    #[serde(default, deserialize_with = "lenient_capabilities")]
    pub live_capabilities: Capabilities,
    /// Protocol version the gateway speaks (5.0+). `None` if absent, `null` or not a `u32`.
    #[serde(default, deserialize_with = "lenient_u32")]
    pub protocol: Option<u32>,
}

// --- tolerant `ready` fields without an intermediate tree (round-2 review F5) ----------------
// The first version decoded each field into a `serde_json::Value` and then tried `T`: a 1 MiB
// `ready` frame of `[[],[],...]` peaked at 16 MiB. These visitors take what they understand
// straight from the input and skip everything else with `IgnoredAny` (no allocation); a value of
// the wrong shape becomes the default. Only the strings that are actually kept are allocated.

/// Visits one JSON value: `Some(T)` if it is a string `T` accepts, `None` for anything else.
struct Lenient<T>(PhantomData<T>);

/// How a string becomes the field's type.
trait FromJsonStr: Sized {
    fn from_json_str<E: de::Error>(s: &str) -> Option<Self>;
}

impl FromJsonStr for String {
    fn from_json_str<E: de::Error>(s: &str) -> Option<Self> {
        Some(String::from(s))
    }
}

impl FromJsonStr for InputMode {
    fn from_json_str<E: de::Error>(s: &str) -> Option<Self> {
        InputMode::deserialize(StrDeserializer::<E>::new(s)).ok()
    }
}

impl FromJsonStr for OutputMode {
    fn from_json_str<E: de::Error>(s: &str) -> Option<Self> {
        OutputMode::deserialize(StrDeserializer::<E>::new(s)).ok()
    }
}

impl FromJsonStr for TurnOrigin {
    fn from_json_str<E: de::Error>(s: &str) -> Option<Self> {
        TurnOrigin::deserialize(StrDeserializer::<E>::new(s)).ok()
    }
}

impl FromJsonStr for TurnEndReason {
    /// Unknown strings are `Error` (spec M19 §5); non-strings stay `None`.
    fn from_json_str<E: de::Error>(s: &str) -> Option<Self> {
        Some(
            TurnEndReason::deserialize(StrDeserializer::<E>::new(s))
                .unwrap_or(TurnEndReason::Error),
        )
    }
}

fn lenient_origin<'de, D: Deserializer<'de>>(d: D) -> Result<Option<TurnOrigin>, D::Error> {
    d.deserialize_any(Lenient::<TurnOrigin>(PhantomData))
}

fn lenient_end_reason<'de, D: Deserializer<'de>>(d: D) -> Result<Option<TurnEndReason>, D::Error> {
    d.deserialize_any(Lenient::<TurnEndReason>(PhantomData))
}

fn drain_seq<'de, A: SeqAccess<'de>>(mut a: A) -> Result<(), A::Error> {
    while a.next_element::<IgnoredAny>()?.is_some() {}
    Ok(())
}

fn drain_map<'de, A: MapAccess<'de>>(mut a: A) -> Result<(), A::Error> {
    while a.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
    Ok(())
}

/// Scalars and containers that are not what the field wants: skipped, `default` returned.
macro_rules! lenient_other {
    ($default:expr) => {
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok($default)
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok($default)
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok($default)
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok($default)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok($default)
        }
        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok($default)
        }
    };
}

impl<'de, T: FromJsonStr> Visitor<'de> for Lenient<T> {
    type Value = Option<T>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(T::from_json_str::<E>(v))
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, a: A) -> Result<Self::Value, A::Error> {
        drain_seq(a)?;
        Ok(None)
    }
    fn visit_map<A: MapAccess<'de>>(self, a: A) -> Result<Self::Value, A::Error> {
        drain_map(a)?;
        Ok(None)
    }
    lenient_other!(None);
}

impl<'de, T: FromJsonStr> de::DeserializeSeed<'de> for Lenient<T> {
    type Value = Option<T>;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

fn lenient_string<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(d.deserialize_any(Lenient::<String>(PhantomData))?
        .unwrap_or_default())
}

fn lenient_opt_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    d.deserialize_any(Lenient::<String>(PhantomData))
}

/// A JSON integer that fits `u32`; anything else is `None`.
struct LenientU32;

impl<'de> Visitor<'de> for LenientU32 {
    type Value = Option<u32>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        Ok(u32::try_from(v).ok())
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
        Ok(u32::try_from(v).ok())
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, a: A) -> Result<Self::Value, A::Error> {
        drain_seq(a)?;
        Ok(None)
    }
    fn visit_map<A: MapAccess<'de>>(self, a: A) -> Result<Self::Value, A::Error> {
        drain_map(a)?;
        Ok(None)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
}

fn lenient_u32<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    d.deserialize_any(LenientU32)
}

fn lenient_input_mode<'de, D: Deserializer<'de>>(d: D) -> Result<Option<InputMode>, D::Error> {
    d.deserialize_any(Lenient::<InputMode>(PhantomData))
}

/// Known output modes of an array; anything else (unknown strings, other values, a non-array)
/// is skipped.
struct LenientModes;

impl<'de> Visitor<'de> for LenientModes {
    type Value = Vec<OutputMode>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(m) = a.next_element_seed(Lenient::<OutputMode>(PhantomData))? {
            // At most one entry per known mode: a frame of `"audio","audio",...` stays tiny.
            if let Some(m) = m.filter(|m| !out.contains(m)) {
                out.push(m);
            }
        }
        Ok(out)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
    fn visit_map<A: MapAccess<'de>>(self, a: A) -> Result<Self::Value, A::Error> {
        drain_map(a)?;
        Ok(Vec::new())
    }
    lenient_other!(Vec::new());
}

fn lenient_modes<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<OutputMode>, D::Error> {
    d.deserialize_any(LenientModes)
}

/// A capabilities object: known keys with boolean values are taken; a known key with another
/// value counts as `false`; unknown keys are skipped; a non-object gives the default.
struct LenientCaps;

/// Which capability a key names (`None`: unknown key).
struct CapKey;

impl<'de> Visitor<'de> for CapKey {
    type Value = Option<u8>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a key")
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(match v {
            "barge_in" => Some(0),
            "tts" => Some(1),
            "transcriber" => Some(2),
            _ => None,
        })
    }
}

impl<'de> de::DeserializeSeed<'de> for CapKey {
    type Value = Option<u8>;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_str(self)
    }
}

/// A boolean, or `false` for any other value (skipped).
struct LenientBool;

impl<'de> Visitor<'de> for LenientBool {
    type Value = bool;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
        Ok(v)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(false)
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(false)
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(false)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(false)
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(false)
    }
    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(false)
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, a: A) -> Result<Self::Value, A::Error> {
        drain_seq(a)?;
        Ok(false)
    }
    fn visit_map<A: MapAccess<'de>>(self, a: A) -> Result<Self::Value, A::Error> {
        drain_map(a)?;
        Ok(false)
    }
}

impl<'de> de::DeserializeSeed<'de> for LenientBool {
    type Value = bool;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for LenientCaps {
    type Value = Capabilities;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
        let mut c = Capabilities::default();
        while let Some(k) = a.next_key_seed(CapKey)? {
            match k {
                Some(0) => c.barge_in = a.next_value_seed(LenientBool)?,
                Some(1) => c.tts = a.next_value_seed(LenientBool)?,
                Some(2) => c.transcriber = a.next_value_seed(LenientBool)?,
                _ => {
                    a.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(c)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(Capabilities::default())
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, a: A) -> Result<Self::Value, A::Error> {
        drain_seq(a)?;
        Ok(Capabilities::default())
    }
    lenient_other!(Capabilities::default());
}

fn lenient_capabilities<'de, D: Deserializer<'de>>(d: D) -> Result<Capabilities, D::Error> {
    d.deserialize_any(LenientCaps)
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
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transcription {
    pub text: String,
}

/// `<redacted N bytes>`: user content never reaches `{:?}` (review sec#7, LOG-1).
struct Redacted(usize);

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<redacted {} bytes>", self.0)
    }
}

impl fmt::Debug for Transcription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transcription")
            .field("text", &Redacted(self.text.len()))
            .finish()
    }
}

/// CERTAIN names. `turn_seq` = N & 0xFFFF (wraps; compare by equality). `turn_id` is "t-N" and
/// restarts at t-1 on every connection. `origin` and `conversation` are new in gateway M19
/// (absent from older gateways, so both optional and lenient).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStart {
    pub turn_id: String,
    pub turn_seq: u16,
    /// M19: `"user"` or `"push"`; `None` from older gateways or for an unknown value.
    #[serde(
        default,
        deserialize_with = "lenient_origin",
        skip_serializing_if = "Option::is_none"
    )]
    pub origin: Option<TurnOrigin>,
    /// M19: the conversation id this turn runs in (changes at the daily cut-over).
    #[serde(
        default,
        deserialize_with = "lenient_opt_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub conversation: Option<String>,
}

/// M19 `turn_start.origin`. CERTAIN values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnOrigin {
    User,
    Push,
}

/// CERTAIN (3.0.1 §5). Unknown phases are a generic «working» (the list may grow).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProgressPhase {
    Preparing,
    Thinking,
    Tool,
    /// Any phase this client does not know, or a missing/odd `phase`.
    Working,
}

impl FromJsonStr for ProgressPhase {
    fn from_json_str<E: de::Error>(s: &str) -> Option<Self> {
        Some(
            ProgressPhase::deserialize(StrDeserializer::<E>::new(s))
                .unwrap_or(ProgressPhase::Working),
        )
    }
}

fn lenient_phase<'de, D: Deserializer<'de>>(d: D) -> Result<ProgressPhase, D::Error> {
    Ok(d.deserialize_any(Lenient::<ProgressPhase>(PhantomData))?
        .unwrap_or(ProgressPhase::Working))
}

/// CERTAIN (3.0.1 §5). `tool` is a tool NAME only (never arguments or results); `None` outside
/// `phase: tool` or when the gateway withheld a non-identifier name. A name that breaks the
/// gateway's own rule (`[A-Za-z0-9_.:-]`, at most 128 chars) is dropped to `None` («una
/// herramienta»), so it can never carry text or grow the UI's view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnProgress {
    #[serde(default, deserialize_with = "lenient_string")]
    pub turn_id: String,
    #[serde(default = "working", deserialize_with = "lenient_phase")]
    pub phase: ProgressPhase,
    #[serde(default, deserialize_with = "lenient_tool")]
    pub tool: Option<String>,
}

/// Longest tool name the gateway sends (3.0.1 §5).
pub const TOOL_NAME_MAX: usize = 128;

fn working() -> ProgressPhase {
    ProgressPhase::Working
}

fn lenient_tool<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(lenient_opt_string(d)?.filter(|t| {
        !t.is_empty()
            && t.len() <= TOOL_NAME_MAX
            && t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
    }))
}

/// CERTAIN
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub turn_id: String,
    pub text: String,
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Token")
            .field("turn_id", &self.turn_id)
            .field("text", &Redacted(self.text.len()))
            .finish()
    }
}

/// CERTAIN: exactly one per TurnStart (known exception JGW-286: silent push). `reason` is new in
/// gateway M19 and replaces the separate `interrupted` message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEnd {
    pub turn_id: String,
    /// M19; `None` from older gateways. An unknown value decodes as `Error` (spec: "treat any
    /// other value as error: the list can grow").
    #[serde(
        default,
        deserialize_with = "lenient_end_reason",
        skip_serializing_if = "Option::is_none"
    )]
    pub reason: Option<TurnEndReason>,
}

/// M19 `turn_end.reason`. CERTAIN values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnEndReason {
    Done,
    Error,
    Cancelled,
    /// Barge-in or a final transcription during the turn (was the `interrupted` message).
    Interrupted,
    /// A new `send` replaced it.
    Replaced,
    Timeout,
    Closed,
}

impl TurnEndReason {
    /// The turn was cut short in favour of the user, or its session is closing (`closed` comes
    /// right before a 4011/4020/4200 close): stop what is still playing.
    pub fn stops_playback(self) -> bool {
        matches!(
            self,
            TurnEndReason::Interrupted | TurnEndReason::Replaced | TurnEndReason::Closed
        )
    }
}

/// Gateways v1.19.4 to 2.x; arrives after the cancelled turn's TurnEnd. Gone in M19 (now
/// `turn_end.reason = "interrupted"`); kept for gateways not yet on M19.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interrupted {
    #[serde(default)]
    pub turn_id: Option<String>,
}

/// Field names CERTAIN (JAP §1.6). `args` shape UNKNOWN and unused: skipped, not kept (sec#4).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
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

impl fmt::Debug for ToolCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolCall")
            .field("turn_id", &self.turn_id)
            .field("phase", &self.phase)
            .field("name", &self.name)
            .field("tool_call_id", &self.tool_call_id)
            .field("args", &self.args)
            .field("result", &self.result.as_ref().map(|r| Redacted(r.len())))
            .field("is_error", &self.is_error)
            .finish()
    }
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
    // CERTAIN (5.0, #417)
    OrchestratorAuthFailed,
    OrchestratorModelUnavailable,
    OrchestratorRateLimited,
    OrchestratorTimeout,
    OrchestratorAgentUnavailable,
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
            "orchestrator_auth_failed" => TurnErrorReason::OrchestratorAuthFailed,
            "orchestrator_model_unavailable" => TurnErrorReason::OrchestratorModelUnavailable,
            "orchestrator_rate_limited" => TurnErrorReason::OrchestratorRateLimited,
            "orchestrator_timeout" => TurnErrorReason::OrchestratorTimeout,
            "orchestrator_agent_unavailable" => TurnErrorReason::OrchestratorAgentUnavailable,
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
    /// CERTAIN (3.0.1 §15): ms since the session's PREVIOUS stage, whatever it was (not since
    /// the turn's start). To measure a span, add the stages that make it up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}
