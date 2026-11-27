//! fogar-proto: the fogar-gateway client protocol **v1 ("v1 with fixes")**, as reconstructed in
//! `PROTOCOL-SPEC.md` / `RUST-MODEL.md` (vault snapshot 2026-11-13). Protocol v2 does not exist on
//! the wire and is deliberately not modelled.
//!
//! Certainty marks on items mirror RUST-MODEL.md: **CERTAIN**, **LIKELY**, **GUESSED**.
//! GUESSED shapes are kept loose (raw `String` / `serde_json::Value`) so we never invent fields.
//!
//! Rules:
//! - `#![no_std]` + `alloc`; no I/O, no async, no OS, no clocks (archtest R5/R6, clippy.toml).
//! - Tolerant reader: every server enum has an escape hatch; unknown fields are ignored.
//! - Strict writer: client messages carry only documented fields.
//! - Binary audio parsing is zero-copy and allocation-free.
//! - Malformed input returns an error, never panics (proptest-checked).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

pub mod account;
pub mod audio;
pub mod client;
pub mod close;
pub mod conversations;
pub mod events;
pub mod pairing;
pub mod server;
pub mod sse;

mod codec;

pub use audio::{
    AudioFrameError, TtsFrame, decode_upload_f32le, encode_tts_frame, encode_upload_f32le,
    parse_tts_frame,
};
pub use client::{ClientMsg, Handshake, InputMode, OutputMode};
pub use close::{CloseAction, classify_close};
pub use codec::{
    DecodeError, EncodeError, decode_client, decode_server, encode_client, encode_handshake,
};
pub use server::ServerMsg;

/// Timing and size constants from the spec (not announced at runtime by the gateway).
pub mod limits {
    /// CERTAIN (v1.19.5): the handshake must arrive within 5 s of the WS upgrade.
    pub const HANDSHAKE_TIMEOUT_S: u32 = 5;
    /// CERTAIN: max handshake length, in **characters**.
    pub const HANDSHAKE_MAX_CHARS: usize = 4096;
    /// CERTAIN: max inbound WS frame on the gateway (uvicorn `ws_max_size`). Larger => close 1009.
    pub const WS_MAX_FRAME_BYTES: usize = 1_048_576;
    /// CERTAIN: 300 s without client messages => close 1000, no warning. Paused during turns.
    pub const IDLE_TIMEOUT_S: u32 = 300;
    /// LIKELY good value (jota-voice practice).
    pub const KEEPALIVE_EVERY_S: u32 = 120;
    /// CERTAIN: server turn timeout, reset on every event.
    pub const TURN_TIMEOUT_S: u32 = 120;
    /// CERTAIN: a client that blocks a server send for longer is closed with 1013.
    pub const CLIENT_SEND_TIMEOUT_S: u32 = 10;
    /// CERTAIN: key revalidation period on the gateway.
    pub const REVALIDATE_INTERVAL_S: u32 = 10;
    /// CERTAIN: pairing code TTL.
    pub const PAIR_CODE_TTL_S: u32 = 600;
    /// jota-voice practice: reconnect ceilings (we add full jitter, see fogar-core backoff).
    pub const RECONNECT_BACKOFF_S: [u32; 5] = [1, 2, 5, 10, 30];
}
