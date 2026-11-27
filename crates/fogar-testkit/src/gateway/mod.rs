//! Mock gateway for protocol v1, at gateway 5.0. One connection driver (`driver.rs`) implements
//! the server side of the protocol as PROTOCOL-SPEC.md describes it; two transports run it:
//! - [`MockGateway`]: a real local WebSocket server (tokio-tungstenite) for end-to-end tests;
//! - [`FakeGatewayLink`]: an in-memory `GatewayLink` for paused-time tests (reconnect/backoff,
//!   72 h soak), where real sockets would race the auto-advancing clock.
//!
//! Server behaviour modelled (spec section in brackets): handshake without `type`, 5 s timeout,
//! 4096-char limit, `protocol: 5` (close 4003, checked before the key; `ready` repeats it, 5.0
//! #417), credential in `token` (a handshake without it, e.g. the 4.x `client_key`, is 4000),
//! key check (4010; the exact 1008 reasons with `m19 = false`) then 4011 `missing scope` when
//! `MockConfig::missing_scope` [§1, §13]; `POST /auth/login` is scripted per test; optional `status` before
//! `ready` [§5]; `end` -> `transcription_partial` + `transcription` [§7]; `send` replaces the
//! active turn [§9]; `turn_start`/`token`/paced `0xA1` audio/`turn_end` [§7, §12]; `cancel` ->
//! `turn_end` [§6]; idle close 1000 after client silence [§13]; malformed JSON ->
//! `INVALID_MESSAGE` (non-fatal) [§6]; scripted closes with any code/reason and abrupt drops.
//! The REST surface (`/.well-known/fogar-gateway`, `/auth/*`) is served on the same port as
//! scripted [`HttpRoutes`] (`MockGateway::routes`).
//! A push waits behind the user's streaming turn; a push arriving while another push is open is
//! dropped (3.0.1 §10: another conversation's push; the same conversation's would fold in).
//! The chat over HTTP (5.0, §16) is [`EventsHub`]: a stateful `GET /events` + conversations
//! gateway installed into the same `HttpRoutes` (`EventsHub::install`), so `FakeGatewayHttp`
//! and `MockHttpServer` serve it alike.
//! NOT modelled: STT/barge-in from audio partials, conversations, tool calls, pipeline events,
//! `phrase_interrupted`.
//!
//! The spec this mock follows is pinned below; `tests/spec_pin.rs` fails when a gateway checkout
//! (`FOGAR_GATEWAY_DIR`) has a different `docs/client-protocol.md`: review the diff, update the
//! mock, then bump both constants.

/// fogar-gateway commit (`origin/next/5.0`, gateway 5.0) whose `docs/client-protocol.md` this mock was last
/// checked against.
pub const SPEC_GATEWAY_COMMIT: &str = "290efa0f8df7c72dec453ca491b6117225f5a2f9";
/// `git hash-object docs/client-protocol.md` at [`SPEC_GATEWAY_COMMIT`].
pub const SPEC_BLOB: &str = "55168f0e78f4f563a4e5c05efcd692541a7582a2";

mod driver;
mod events;
mod http;
mod memory;
mod ws;

pub use driver::{
    Ctl, GatewayLog, MockConfig, REASON_BAD_JSON, REASON_BAD_KEY, REASON_NOT_TEXT, REASON_PROTOCOL,
    REASON_TIMEOUT, REASON_TOO_LARGE,
};
pub use events::{EventsHub, HubConfig, QUEUE_MAX, RING_MAX, Scope};
pub use http::{
    FakeGatewayHttp, FakeStreamBody, HttpRoutes, MockHttpServer, SeenRequest, StreamStep,
};
pub use memory::{FAKE_MAX_FRAME, FakeGatewayLink, FakeSession};
pub use ws::MockGateway;

/// Which `WsRoute` each accepted connection used, oldest first.
pub trait RouteLog {
    fn routes_used(&self) -> Vec<fogar_ports::gateway::WsRoute>;
}
