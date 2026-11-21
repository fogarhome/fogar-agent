//! JSON text-frame codecs. Errors are small `Copy` enums (no allocation, no source chains that
//! could echo untrusted input into logs).

use alloc::{borrow::Cow, string::String};
use core::fmt;
use serde::Deserialize;

use crate::{
    client::{ClientMsg, Handshake},
    limits::{HANDSHAKE_MAX_CHARS, WS_MAX_FRAME_BYTES},
    server::ServerMsg,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// Larger than the WS max frame: never parsed.
    TooLarge { len: usize },
    /// Not JSON, not an object, missing `type`, or a field with the wrong type.
    Invalid,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::TooLarge { len } => {
                write!(f, "text frame of {len} bytes exceeds the limit")
            }
            DecodeError::Invalid => f.write_str("text frame is not a valid protocol message"),
        }
    }
}
impl core::error::Error for DecodeError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EncodeError {
    /// Handshake longer than `HANDSHAKE_MAX_CHARS` characters (gateway would close 1008).
    HandshakeTooLarge { chars: usize },
    /// Frame larger than `WS_MAX_FRAME_BYTES` (gateway would close 1009).
    TooLarge { len: usize },
    /// Serialisation failed (cannot happen for these types; kept for totality).
    Serialize,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::HandshakeTooLarge { chars } => {
                write!(f, "handshake has {chars} chars (max {HANDSHAKE_MAX_CHARS})")
            }
            EncodeError::TooLarge { len } => write!(f, "frame of {len} bytes exceeds 1 MiB"),
            EncodeError::Serialize => f.write_str("serialisation failed"),
        }
    }
}
impl core::error::Error for EncodeError {}

/// Only the `type` tag; every other field is skipped without allocating.
#[derive(Deserialize)]
struct Tag<'a> {
    #[serde(borrow, rename = "type")]
    ty: Cow<'a, str>,
}

/// Decodes one server text frame. Never panics on any input (proptest-checked).
///
/// Two passes over the text instead of serde's internally-tagged enum (review sec#4): the derived
/// `ServerMsg` deserializer buffers the WHOLE object as a `Content` tree before it can look at
/// `type` (~32x the frame size for nested arrays). Pass 1 reads only `type`; pass 2 parses the
/// frame straight into the variant's struct, skipping unknown fields. Unknown types never parse
/// further than the tag.
pub fn decode_server(text: &str) -> Result<ServerMsg, DecodeError> {
    if text.len() > WS_MAX_FRAME_BYTES {
        return Err(DecodeError::TooLarge { len: text.len() });
    }
    fn body<'a, T: Deserialize<'a>>(text: &'a str) -> Result<T, DecodeError> {
        serde_json::from_str(text).map_err(|_| DecodeError::Invalid)
    }
    let tag: Tag<'_> = body(text)?;
    Ok(match tag.ty.as_ref() {
        "ready" => ServerMsg::Ready(body(text)?),
        "transcription_partial" => ServerMsg::TranscriptionPartial(body(text)?),
        "transcription" => ServerMsg::Transcription(body(text)?),
        "turn_start" => ServerMsg::TurnStart(body(text)?),
        "token" => ServerMsg::Token(body(text)?),
        "tool_call" => ServerMsg::ToolCall(body(text)?),
        "turn_end" => ServerMsg::TurnEnd(body(text)?),
        "interrupted" => ServerMsg::Interrupted(body(text)?),
        "status" => ServerMsg::Status(body(text)?),
        "error" => ServerMsg::Error(body(text)?),
        "pipeline_event" => ServerMsg::PipelineEvent(body(text)?),
        "turn_progress" => ServerMsg::TurnProgress(body(text)?),
        _ => ServerMsg::Unknown,
    })
}

/// Decodes one client text frame (used by the fake gateway; the agent never needs it).
pub fn decode_client(text: &str) -> Result<ClientMsg<'_>, DecodeError> {
    if text.len() > WS_MAX_FRAME_BYTES {
        return Err(DecodeError::TooLarge { len: text.len() });
    }
    serde_json::from_str(text).map_err(|_| DecodeError::Invalid)
}

/// Encodes a client message as a JSON text frame.
pub fn encode_client(msg: &ClientMsg<'_>) -> Result<String, EncodeError> {
    let s = serde_json::to_string(msg).map_err(|_| EncodeError::Serialize)?;
    if s.len() > WS_MAX_FRAME_BYTES {
        return Err(EncodeError::TooLarge { len: s.len() });
    }
    Ok(s)
}

/// Encodes the handshake and enforces the gateway's 4096-character limit.
pub fn encode_handshake(h: &Handshake<'_>) -> Result<String, EncodeError> {
    let s = serde_json::to_string(h).map_err(|_| EncodeError::Serialize)?;
    let chars = s.chars().count();
    if chars > HANDSHAKE_MAX_CHARS {
        return Err(EncodeError::HandshakeTooLarge { chars });
    }
    Ok(s)
}
