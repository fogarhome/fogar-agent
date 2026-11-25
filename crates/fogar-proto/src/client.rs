//! Client -> server messages (spec §4, §6).
//!
//! Deviation from RUST-MODEL.md (ADR-0006): string fields are `Cow<'a, str>` instead of `&'a str`
//! so the same types can also be *decoded* (the fake gateway in fogar-testkit parses client
//! frames). Encoding still borrows and does not copy the strings.

use alloc::{borrow::Cow, vec::Vec};
use core::fmt;
use serde::{Deserialize, Serialize};

/// Gateway protocol version this client speaks. CERTAIN (5.0, #417): the handshake must carry it
/// or the gateway closes 4003.
pub const PROTOCOL_VERSION: u32 = 5;

/// First client frame. **No `type` tag.** CERTAIN (JAP §1.3; Doc-vs-code P02/P03).
/// The gateway has `extra="allow"`, but we send only documented fields (strict writer).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handshake<'a> {
    #[serde(borrow)]
    pub client_key: Cow<'a, str>,
    /// Always serialized; writers set `PROTOCOL_VERSION`. CERTAIN (5.0, #417). A decoder (the
    /// fake gateway) reads a missing one as 0, which a 5.0 gateway refuses with 4003.
    #[serde(default)]
    pub protocol: u32,
    pub input_mode: InputMode,
    /// At least one; governs the session. We never request `status` by default (spec §7).
    pub output_mode: Vec<OutputMode>,
    #[serde(borrow, default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<Cow<'a, str>>,
}

/// Redacting `Debug`: the client key is a bearer credential (fogar-secure-rust LOG-1).
impl fmt::Debug for Handshake<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handshake")
            .field("client_key", &"<redacted>")
            .field("protocol", &self.protocol)
            .field("input_mode", &self.input_mode)
            .field("output_mode", &self.output_mode)
            .field("agent", &self.agent)
            .finish()
    }
}

/// CERTAIN.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputMode {
    Audio,
    Text,
}

/// CERTAIN. `Status` enables `pipeline_event` (not `status` messages, which are always sent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputMode {
    Audio,
    Text,
    Status,
}

/// CERTAIN for End/Send/Cancel. `Keepalive` is LIKELY (any unknown `type` resets idle; jota-voice
/// sends exactly `{"type":"keepalive"}`). Plain-text prompts are deliberately not modelled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg<'a> {
    /// End of utterance (audio mode). A final `transcription` follows.
    End,
    /// Opens a turn; REPLACES any active turn and cuts an open push.
    Send {
        #[serde(borrow)]
        text: Cow<'a, str>,
    },
    /// Cancels the active turn and any open push. No `turn_id` argument exists today.
    Cancel,
    /// Resets IDLE_TIMEOUT_S. No reply.
    Keepalive,
}
