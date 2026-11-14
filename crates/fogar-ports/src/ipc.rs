//! IPC server port: the local UI (Swift app on macOS) talks to the agent over a local socket.
//! ADR-0002: Unix domain socket, not XPC. This port is the transport + peer identity only; the
//! request vocabulary is the small closed enum below (fogar-secure-rust IPC-4: no "run arbitrary
//! command/path" method, typed messages, bounded frames).
//!
//! - ASYNC (accept and request/response latency are real I/O). Not dyn-compatible: the core is
//!   generic over `S: IpcServer`.
//! - `accept` and `IpcConn::recv` MUST be **cancel-safe** (they are `select!` branches).
//! - Adapters authenticate the peer BEFORE returning from `accept` (uid via `SO_PEERCRED` /
//!   `getpeereid`; on macOS also the audit token -> SecCode designated requirement). A peer that
//!   fails is dropped inside the adapter and never reaches the core.
//! - Adapters bound frame size (`MAX_FRAME_BYTES`) and reject unknown fields.
//!
//! How Windows/Linux would do this: Linux UDS in `$XDG_RUNTIME_DIR` + `SO_PEERCRED`;
//! Windows named pipe with a user-SID DACL, `reject_remote_clients(true)`. Same port.

use crate::Support;
use std::future::Future;

/// Largest IPC frame an adapter accepts or sends.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct IpcCaps {
    /// The adapter verifies the peer's OS user.
    pub peer_uid: Support,
    /// The adapter verifies the peer's code signature (macOS audit token + SecCode).
    pub peer_code_identity: Support,
}

impl IpcCaps {
    pub const NONE: Self = Self {
        peer_uid: Support::Unsupported,
        peer_code_identity: Support::Unsupported,
    };
}

/// Who is on the other end, as verified by the adapter. Never derived from message content.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PeerIdentity {
    pub uid: Option<u32>,
    /// Code-signing identifier when verified (macOS); `None` otherwise.
    pub code_id: Option<String>,
}

impl PeerIdentity {
    pub fn new(uid: Option<u32>, code_id: Option<String>) -> Self {
        Self { uid, code_id }
    }
}

/// Requests a UI may send. Closed set (no generic "exec").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IpcRequest {
    /// Version handshake: the UI states the IPC protocol version it speaks.
    Hello { version: u32 },
    /// Current health snapshot.
    Status,
}

/// Plain status data for the UI. Contains no secrets and no user content.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StatusReport {
    pub link: String,
    pub sessions: u64,
    pub turns_completed: u64,
    pub boot_mode: String,
}

impl StatusReport {
    pub fn new(link: String, sessions: u64, turns_completed: u64, boot_mode: String) -> Self {
        Self {
            link,
            sessions,
            turns_completed,
            boot_mode,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IpcResponse {
    Hello {
        version: u32,
    },
    Status(StatusReport),
    /// The request was understood but refused (e.g. version mismatch).
    Refused {
        reason: &'static str,
    },
}

/// IPC protocol version spoken by this build.
pub const IPC_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum IpcError {
    #[error("not supported by this adapter: {0}")]
    Unsupported(&'static str),
    #[error("frame too large: {len} > {max}")]
    TooLarge { len: usize, max: usize },
    #[error("malformed frame")]
    Malformed,
    #[error("ipc transport failure")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync>),
}

pub trait IpcConn: Send + 'static {
    /// Next request; `Ok(None)` = the peer closed. **Cancel-safe.**
    fn recv(&mut self) -> impl Future<Output = Result<Option<IpcRequest>, IpcError>> + Send;
    /// Sends one response. Not a select branch; the core wraps it in a deadline.
    fn send(&mut self, resp: IpcResponse) -> impl Future<Output = Result<(), IpcError>> + Send;
}

pub trait IpcServer: Send + Sync + 'static {
    type Conn: IpcConn;
    fn capabilities(&self) -> IpcCaps;
    /// Next authenticated connection. **Cancel-safe.**
    fn accept(&self) -> impl Future<Output = Result<(PeerIdentity, Self::Conn), IpcError>> + Send;
}
