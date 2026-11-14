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

/// A 6-digit pairing code as typed by the person (spaces allowed, client-protocol §0). One-use
/// and short-lived, but still a credential while open: redacted `Debug`, wiped on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct PairCode(secrecy::zeroize::Zeroizing<String>);

impl PairCode {
    /// Keeps the code only if it is exactly 6 ASCII digits once spaces are removed, so a typo
    /// never spends one of the gateway's 5 attempts.
    pub fn parse(typed: &str) -> Option<Self> {
        let digits: String = typed.chars().filter(|c| *c != ' ').collect();
        (digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| Self(secrecy::zeroize::Zeroizing::new(digits)))
    }
    pub fn digits(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for PairCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairCode(<redacted>)")
    }
}

/// Requests a UI may send. Closed set (no generic "exec"). The address in `Pair` is the person's
/// raw input; the core validates it (`GatewayAddr::parse`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IpcRequest {
    /// Version handshake: the UI states the IPC protocol version it speaks.
    Hello { version: u32 },
    /// Current health snapshot.
    Status,
    /// From now on the agent pushes `IpcResponse::Event` with the latest status whenever it
    /// changes (latest value only, never a backlog). Answered with `Subscribed` + one `Event`.
    /// A subscribed connection is not closed for being idle.
    Subscribe,
    /// Look for gateways (saved address, this Mac, later the tailnet). Answered with `Discovered`.
    Discover,
    /// Redeem a pairing code at a gateway. Answered with `Paired` or `PairFailed`. `code` is
    /// `None` when what was typed is not 6 digits: the core answers `BadCode` without calling
    /// the gateway.
    Pair {
        address: String,
        code: Option<PairCode>,
    },
    /// Forget the pairing (delete the record) and stop the link. Answered with `Unpaired`.
    Unpair,
}

/// Link state as the UI shows it. Mirrors fogar-core's link states without their internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkPhase {
    /// No pairing record: the UI shows the pairing screen.
    NotPaired,
    Connecting,
    Ready,
    /// Waiting before the next attempt.
    Reconnecting,
    /// The gateway refused us in a way retrying will not fix (bad config, client bug).
    Parked,
    Stopping,
}

/// The gateway the agent is paired with. Never carries the key.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GatewayInfo {
    /// `http(s)://host:port` form of the address.
    pub address: String,
    /// The gateway's `name` (e.g. "green-house").
    pub name: String,
    /// Gateway version from the last `/.well-known` or `ready`; empty when unknown.
    pub version: String,
    /// The name this client was registered with (the admin may have overridden ours).
    pub client_name: String,
}

impl GatewayInfo {
    pub fn new(address: String, name: String, version: String, client_name: String) -> Self {
        Self {
            address,
            name,
            version,
            client_name,
        }
    }
}

/// How the IPC adapter authenticated this UI (shown honestly in the app).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerCheck {
    /// Same OS user only (Linux; unsigned/ad-hoc builds on macOS).
    UidOnly,
    /// Same user AND same code signer as the agent + expected bundle id (macOS).
    SameSigner,
}

/// Plain status data for the UI. Contains no secrets and no user content.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StatusReport {
    /// Human-readable link label (as in slice 0), for logs and the CLI.
    pub link: String,
    pub phase: LinkPhase,
    /// `None` when not paired.
    pub gateway: Option<GatewayInfo>,
    pub sessions: u64,
    pub turns_completed: u64,
    pub boot_mode: String,
    pub peer_check: PeerCheck,
}

impl StatusReport {
    pub fn new(
        link: String,
        phase: LinkPhase,
        gateway: Option<GatewayInfo>,
        sessions: u64,
        turns_completed: u64,
        boot_mode: String,
        peer_check: PeerCheck,
    ) -> Self {
        Self {
            link,
            phase,
            gateway,
            sessions,
            turns_completed,
            boot_mode,
            peer_check,
        }
    }
}

/// A gateway found by `Discover`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GatewayCandidate {
    /// `http(s)://host:port`, ready to send back in `Pair`.
    pub address: String,
    pub name: String,
    pub version: String,
}

impl GatewayCandidate {
    pub fn new(address: String, name: String, version: String) -> Self {
        Self {
            address,
            name,
            version,
        }
    }
}

/// Why pairing did not happen. Each maps to one sentence in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PairFailure {
    /// The address does not parse.
    BadAddress,
    /// The code is not 6 digits (never sent to the gateway).
    BadCode,
    /// Nothing answered at that address.
    Unreachable,
    /// Something answered, but it is not a Fogar gateway.
    NotAGateway,
    /// 400: unknown, expired, used or mistyped code. "Pide otro código si estás seguro."
    InvalidCode,
    /// 5xx / no answer after sending: the code may still be open; retry with the same code.
    GatewayError,
    /// The key arrived but could not be stored: it is lost, ask for a new code.
    StorageFailed,
    /// Already paired: unpair first.
    AlreadyPaired,
    /// Another pairing is in progress.
    Busy,
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
    Subscribed,
    /// Pushed WITHOUT a request after `Subscribe`: the latest status.
    Event(StatusReport),
    Discovered(Vec<GatewayCandidate>),
    Paired(GatewayInfo),
    PairFailed(PairFailure),
    Unpaired,
}

/// IPC protocol version spoken by this build. 2 = S1 (subscribe, discover, pair, unpair).
pub const IPC_VERSION: u32 = 2;

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
    /// Sends one response. Not a select branch; the core wraps it in a deadline. May be called
    /// without a preceding request (pushed `Event`s after `Subscribe`), and between a `recv`
    /// that was cancelled and the next one: the connection stays usable both ways.
    fn send(&mut self, resp: IpcResponse) -> impl Future<Output = Result<(), IpcError>> + Send;
}

pub trait IpcServer: Send + Sync + 'static {
    type Conn: IpcConn;
    fn capabilities(&self) -> IpcCaps;
    /// Next authenticated connection. **Cancel-safe.**
    fn accept(&self) -> impl Future<Output = Result<(PeerIdentity, Self::Conn), IpcError>> + Send;
}
