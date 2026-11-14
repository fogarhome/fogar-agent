//! Protocol-v1 mock gateway. One connection driver (`driver.rs`) implements the server side of
//! the protocol as PROTOCOL-SPEC.md describes it; two transports run it:
//! - [`MockGateway`]: a real local WebSocket server (tokio-tungstenite) for end-to-end tests;
//! - [`FakeGatewayLink`]: an in-memory `GatewayLink` for paused-time tests (reconnect/backoff,
//!   72 h soak), where real sockets would race the auto-advancing clock.
//!
//! Server behaviour modelled (spec section in brackets): handshake without `type`, 5 s timeout,
//! 4096-char limit, key check with the exact 1008 reasons [§4, §11]; optional `status` before
//! `ready` [§5]; `end` -> `transcription_partial` + `transcription` [§7]; `send` replaces the
//! active turn [§9]; `turn_start`/`token`/paced `0xA1` audio/`turn_end` [§7, §12]; `cancel` ->
//! `turn_end` [§6]; idle close 1000 after client silence [§13]; malformed JSON ->
//! `INVALID_MESSAGE` (non-fatal) [§6]; scripted closes with any code/reason and abrupt drops.
//! NOT modelled: STT/barge-in from audio partials, push queueing, tool calls, pipeline events.

mod driver;
mod memory;
mod ws;

pub use driver::{
    Ctl, GatewayLog, MockConfig, REASON_BAD_JSON, REASON_BAD_KEY, REASON_NOT_TEXT, REASON_TIMEOUT,
    REASON_TOO_LARGE,
};
pub use memory::{FAKE_MAX_FRAME, FakeGatewayLink, FakeSession};
pub use ws::MockGateway;
