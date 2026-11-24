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
//! The REST surface (`/.well-known/fogar-gateway`, `POST /pair`) is served on the same port as
//! scripted [`HttpRoutes`] (`MockGateway::routes`).
//! A push waits behind the user's streaming turn; a push arriving while another push is open is
//! dropped (3.0.1 §10: another conversation's push; the same conversation's would fold in).
//! NOT modelled: STT/barge-in from audio partials, conversations, tool calls, pipeline events,
//! `phrase_interrupted`.
//!
//! The spec this mock follows is pinned below; `tests/spec_pin.rs` fails when a gateway checkout
//! (`FOGAR_GATEWAY_DIR`) has a different `docs/client-protocol.md`: review the diff, update the
//! mock, then bump both constants.

/// fogar-gateway commit (main, release 3.0.1) whose `docs/client-protocol.md` this mock was last
/// checked against.
pub const SPEC_GATEWAY_COMMIT: &str = "d09ce82b9350eb42f6d14c087b00ec0c0104bc6b";
/// `git hash-object docs/client-protocol.md` at [`SPEC_GATEWAY_COMMIT`].
pub const SPEC_BLOB: &str = "2477564d47201206057e139a5b4aece5e3401b94";

mod driver;
mod http;
mod memory;
mod ws;

pub use driver::{
    Ctl, GatewayLog, MockConfig, REASON_BAD_JSON, REASON_BAD_KEY, REASON_NOT_TEXT, REASON_TIMEOUT,
    REASON_TOO_LARGE,
};
pub use http::{
    FakeGatewayHttp, FakeStreamBody, HttpRoutes, MockHttpServer, SeenRequest, StreamStep,
};
pub use memory::{FAKE_MAX_FRAME, FakeGatewayLink, FakeSession};
pub use ws::MockGateway;

/// Which `WsRoute` each accepted connection used, oldest first.
pub trait RouteLog {
    fn routes_used(&self) -> Vec<fogar_ports::gateway::WsRoute>;
}
