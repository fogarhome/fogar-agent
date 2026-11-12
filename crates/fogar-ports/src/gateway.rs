//! GatewayLink port (the uplink): a message-oriented duplex transport to the gateway.
//!
//! The port is the TRANSPORT only (WebSocket semantics: text/binary frames + close code/reason).
//! The protocol (handshake, ready, turns, audio framing) lives in fogar-proto + fogar-core, so
//! the S1 WebSocket adapter cannot drift from the protocol and the fake gateway exercises the
//! real codec.
//!
//! - ASYNC (real network latency), desugared `fn -> impl Future + Send` (see port-design.md).
//!   Not dyn-compatible; the core is generic over `L: GatewayLink`.
//! - `LinkSession::recv` MUST be cancel-safe: dropping the future loses no frame (the core polls
//!   it inside `select!`). `send` and `close` are NOT used as select branches; the core wraps
//!   them in deadlines and treats a dropped send as a dead session.
//! - Every adapter bounds its buffers (inbound frame size, write buffer). No unbounded queues.
//!
//! How Windows/Linux would do this: the same tokio-tungstenite adapter (portable, fogar-host);
//! nothing OS specific. Capabilities therefore stay minimal.

use std::{fmt, future::Future};

/// One WebSocket message.
#[derive(Clone, PartialEq, Eq)]
pub enum Frame {
    Text(String),
    Binary(Vec<u8>),
}

/// Never print payloads (they may carry the client key or user speech): only kind + size.
impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Frame::Text(t) => write!(f, "Text({} bytes)", t.len()),
            Frame::Binary(b) => write!(f, "Binary({} bytes)", b.len()),
        }
    }
}

impl Frame {
    pub fn len(&self) -> usize {
        match self {
            Frame::Text(t) => t.len(),
            Frame::Binary(b) => b.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
    Frame(Frame),
    /// The peer closed with a close frame.
    Closed { code: u16, reason: String },
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LinkError {
    /// No gateway configured / paired: the core parks instead of retrying.
    #[error("gateway link not configured")]
    NotConfigured,
    #[error("not connected")]
    Disconnected,
    #[error("frame too large: {len} > {max}")]
    TooLarge { len: usize, max: usize },
    #[error("transport failure")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync>),
}

pub trait LinkSession: Send + 'static {
    /// Largest frame this transport will send (the gateway accepts 1 MiB).
    fn max_frame(&self) -> usize;
    /// Resolves when the frame is handed to the transport (not when acked).
    /// Oversized frames fail with `TooLarge` and leave the session usable.
    fn send(&mut self, frame: Frame) -> impl Future<Output = Result<(), LinkError>> + Send;
    /// Next inbound event. `None` = the transport died without a close frame (treat as 1006).
    /// **Cancel-safe.**
    fn recv(&mut self) -> impl Future<Output = Option<LinkEvent>> + Send;
    /// Sends a close frame and waits (adapter-bounded) for the peer. Idempotent.
    fn close(&mut self, code: u16, reason: &str) -> impl Future<Output = Result<(), LinkError>> + Send;
}

pub trait GatewayLink: Send + Sync + 'static {
    type Session: LinkSession;
    /// Opens a new transport session (TCP+TLS+WS upgrade for the real adapter). The core wraps
    /// this in its own deadline; dropping the future must not leak a half-open socket.
    fn connect(&self) -> impl Future<Output = Result<Self::Session, LinkError>> + Send;
}
