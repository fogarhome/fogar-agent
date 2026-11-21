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
    /// Private: only [`PeerIdentity::verified_signer`] sets it, so elevating a peer is always an
    /// explicit, greppable act in an adapter.
    code_id: Option<String>,
}

impl PeerIdentity {
    /// A peer authenticated by OS user only.
    pub fn uid_only(uid: Option<u32>) -> Self {
        Self { uid, code_id: None }
    }
    /// A peer the adapter VERIFIED is signed by the same signer as the agent and carries the
    /// expected identifier (ADR-0002 amendment). Grants `PeerCheck::SameSigner`.
    pub fn verified_signer(uid: Option<u32>, code_id: String) -> Self {
        Self {
            uid,
            code_id: Some(code_id),
        }
    }
    pub fn code_id(&self) -> Option<&str> {
        self.code_id.as_deref()
    }
    pub fn check(&self) -> PeerCheck {
        if self.code_id.is_some() {
            PeerCheck::SameSigner
        } else {
            PeerCheck::UidOnly
        }
    }
}

/// A 6-digit pairing code as typed by the person (spaces allowed, client-protocol §0). One-use
/// and short-lived, but still a credential while open: redacted `Debug`, wiped on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct PairCode(secrecy::zeroize::Zeroizing<String>);

impl PairCode {
    /// Keeps the code only if it is exactly 6 ASCII digits once whitespace (any kind, e.g. a
    /// pasted no-break space) and `-` are removed, so a typo never spends one of the gateway's
    /// 5 attempts.
    pub fn parse(typed: &str) -> Option<Self> {
        let digits: String = typed
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .collect();
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
///
/// Authorization (IPC-4): `Pair` and `Unpair` change where the agent sends the user's voice, and
/// `Reconnect` makes the agent spend the stored key to take the conversation back from another
/// session of the user, so the core answers the three only for a `PeerCheck::SameSigner` peer; a
/// `UidOnly` peer gets `Refused` unless the composition root explicitly opts in for development
/// builds. Everything else is read-only and open to any authenticated peer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IpcRequest {
    /// Version handshake: the UI states the IPC protocol version it speaks.
    Hello { version: u32 },
    /// Current health snapshot.
    Status,
    /// From now on the agent pushes `IpcResponse::Event` with the latest status whenever it
    /// changes (latest value only, never a backlog). Answered with `Subscribed` + one `Event`.
    /// A subscribed connection is not closed for being idle, but a pushed `Event` that misses
    /// the write deadline closes it (a client that stops reading cannot hold a slot), and
    /// subscribers share the bounded connection slots (`IPC_MAX_CONNS`).
    Subscribe,
    /// Look for gateways (saved address, this Mac, the tailnet peers). Answered with `Discovered`.
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
    /// The person asks to connect again after the link parked for a reason only they can settle
    /// (gateway close 4102 "conversation opened elsewhere", 4020/4021 agent refused once the
    /// admin fixed it). Answered with `Reconnecting` (one new attempt; progress arrives as status
    /// `Event`s), or `Refused` when the link is not parked for such a reason.
    Reconnect,
    // ---- conversation (v4). SameSigner only, like Pair: they carry or reveal user content. ----
    /// From now on the agent pushes `IpcResponse::Conversation` (latest view only). Answered
    /// with `ConversationSubscribed` + one `Conversation`. Only a subscribed connection may
    /// start a take or confirm a turn as visible.
    SubscribeConversation,
    /// Typed (or reviewed) text. Answered with `TextAccepted` or `TextRefused`.
    SendText { text: UserText },
    /// The person pressed the microphone button. The core's single consent point decides.
    /// Answered with `CaptureStarted` or `CaptureRefused`.
    CaptureStart { gesture: CaptureGesture },
    /// The person ended the take. Answered with `CaptureEnded`.
    CaptureEnd { then: CaptureThen },
    /// Cancel button. Answered with `TurnCancelSent`.
    CancelTurn,
    /// The UI is showing this turn: its audio may play. Answered with `TurnVisibleNoted`.
    TurnVisible { turn_id: String },
    /// The person asked to allow the microphone (may show the system prompt). Answered with
    /// `MicPermission` once decided.
    RequestMicPermission,
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
    /// The gateway refused us in a way retrying on our own will not fix (bad config, client bug,
    /// conversation taken by another session). Some reasons accept a `Reconnect`.
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
    /// The gateway's transcriber and TTS as last heard (`Unknown` until a session is ready).
    pub transcriber: ServiceState,
    pub tts: ServiceState,
}

impl StatusReport {
    pub fn with_voice(mut self, transcriber: ServiceState, tts: ServiceState) -> Self {
        self.transcriber = transcriber;
        self.tts = tts;
        self
    }
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
            transcriber: ServiceState::Unknown,
            tts: ServiceState::Unknown,
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
    /// `Reconnect` accepted: the parked link makes one new attempt.
    Reconnecting,
    /// `SubscribeConversation` accepted; one `Conversation` follows, then one per change.
    ConversationSubscribed,
    /// Pushed WITHOUT a request after `SubscribeConversation`: the latest view (watch, no backlog).
    Conversation(ConversationView),
    /// A take is open: the microphone is on.
    CaptureStarted,
    CaptureRefused(CaptureRefusal),
    /// `CaptureEnd` handled (also when no take was open: ending is idempotent). What happens
    /// to the transcript shows in the next `Conversation`.
    CaptureEnded,
    TextAccepted,
    TextRefused(TextRefusal),
    /// `CancelTurn` handled: `cancel` sent if a turn or a wait was open, audio stopped.
    TurnCancelSent,
    /// `TurnVisible` recorded.
    TurnVisibleNoted,
    /// Answer to `RequestMicPermission` (after the system prompt, if one was shown).
    MicPermission(crate::audio::MicPermission),
}

/// IPC protocol version spoken by this build. 2 = S1 (subscribe, discover, pair, unpair);
/// 3 = reconnect; 4 = conversation (S2).
pub const IPC_VERSION: u32 = 4;

// ---- conversation (IPC v4, spec S2) ------------------------------------------------------
//
// Everything below carries the user's content. The core answers these requests only for a
// `PeerCheck::SameSigner` peer (like Pair), never logs the text, and every type here prints
// sizes, not content, in `Debug`.

/// Largest text a UI may send in one `SendText` (UTF-8 bytes; the core validates).
pub const SEND_TEXT_MAX_BYTES: usize = 8 * 1024;
/// The view keeps at most this many turns (oldest dropped first).
pub const CONVERSATION_MAX_TURNS: usize = 20;
/// ... and at most this much text across them (oldest turns dropped first). The wire codec
/// still guarantees the encoded frame fits `MAX_FRAME_BYTES` whatever the content.
pub const CONVERSATION_MAX_TEXT_BYTES: usize = 48 * 1024;

/// Text the person typed or reviewed. Redacted `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct UserText(pub String);

impl std::fmt::Debug for UserText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "UserText({} B)", self.0.len())
    }
}

/// How the person started a take. For logs and metrics only: the core treats both the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CaptureGesture {
    /// Press and hold; release sends.
    Hold,
    /// Click to start; Stop / Send now / Discard ends it.
    Toggle,
}

/// What happens to the final transcript when the take ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CaptureThen {
    /// Sent with `send`.
    Send,
    /// Goes to the text box (`ConversationView::draft`); never sent on its own.
    Review,
    /// Thrown away.
    Discard,
}

/// Why a take did not open. One sentence each in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CaptureRefusal {
    /// This connection is not subscribed to the conversation (it must show «Escuchando»).
    NotSubscribed,
    /// The OS permission was never asked: offer `RequestMicPermission`.
    PermissionNotDetermined,
    /// Denied or restricted in System Settings.
    PermissionDenied,
    /// This machine cannot capture (no adapter support or no input device).
    NoMicrophone,
    /// The link to the gateway is not ready.
    NotReady,
    /// The gateway has no live transcriber.
    NoTranscriber,
    /// A take is already open.
    Busy,
}

/// Why a `SendText` was not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TextRefusal {
    Empty,
    /// Over `SEND_TEXT_MAX_BYTES`.
    TooLong,
    NotReady,
    /// A take is open: end it first.
    Recording,
}

/// A gateway service as the agent last heard about it (`ready.live_capabilities` + `status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ServiceState {
    #[default]
    Unknown,
    Up,
    Degraded,
    Down,
}

/// What the person is doing, as the core sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum UserActivity {
    #[default]
    Idle,
    /// A take is open (microphone on).
    Recording,
    /// The take ended; waiting for the final transcript.
    Transcribing,
    /// Sent; waiting for the gateway's `turn_start`.
    AwaitingReply,
}

/// The last thing worth telling the person about (cleared by their next action).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Notice {
    /// The transcriber lost the phrase (`phrase_interrupted`): ask to repeat it.
    PhraseLost,
    /// The take was cut because nothing was heard; the transcript is in `draft`.
    SilenceCut,
    /// The take hit the safety limit; the transcript is in `draft`.
    SafetyCut,
}

/// Who opened a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum TurnSource {
    #[default]
    Mine,
    /// Agent-initiated (a reminder): shown as «Fogar te escribió».
    Push,
}

/// Mirrors the gateway's `turn_progress` phases; unknown ones are `Working`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProgressKind {
    Preparing,
    Thinking,
    /// `TurnView::tool` names the tool when the gateway gave a name.
    Tool,
    Working,
}

/// How a turn ended (gateway `turn_end.reason`). `Error` comes with `TurnView::error_code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnOutcome {
    Done,
    Cancelled,
    Interrupted,
    Replaced,
    Timeout,
    Error,
    Closed,
}

/// Whether the reply was heard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum TurnAudio {
    /// No audio (yet, or the turn had none).
    #[default]
    None,
    Played,
    /// Audio arrived but was not played (nobody confirmed the turn was on screen): text only.
    Muted,
}

/// One turn of the conversation. Built by the core; fields are public so it can fill a
/// `default()` value (the struct is `non_exhaustive`).
#[derive(Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct TurnView {
    pub turn_id: String,
    /// The gateway conversation this turn ran in (a change = a new day); `None` when unknown.
    pub conversation: Option<String>,
    pub source: TurnSource,
    /// What I typed or said (empty for a push).
    pub mine: String,
    /// Fogar's reply so far.
    pub reply: String,
    /// Current progress while no text is arriving; `None` once tokens flow or the turn ended.
    pub progress: Option<ProgressKind>,
    pub tool: Option<String>,
    /// `None` while the turn is open.
    pub outcome: Option<TurnOutcome>,
    /// The gateway's stable code when `outcome` is `Error`.
    pub error_code: Option<String>,
    pub audio: TurnAudio,
}

impl std::fmt::Debug for TurnView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnView")
            .field("turn_id", &self.turn_id)
            .field("source", &self.source)
            .field("mine_bytes", &self.mine.len())
            .field("reply_bytes", &self.reply.len())
            .field("progress", &self.progress)
            .field("outcome", &self.outcome)
            .field("audio", &self.audio)
            .finish_non_exhaustive()
    }
}

/// The conversation as the UI draws it: a bounded snapshot pushed latest-only (Decision 3 of
/// spec S2). The UI keeps nothing of its own.
#[derive(Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ConversationView {
    pub activity: UserActivity,
    /// The live `transcription_partial` while recording.
    pub live: String,
    /// The transcript waiting for review in the text box.
    pub draft: Option<String>,
    /// Oldest first, at most `CONVERSATION_MAX_TURNS`.
    pub turns: Vec<TurnView>,
    /// Audio is playing now.
    pub speaking: bool,
    /// A turn whose audio waits for `TurnVisible` before it may play.
    pub awaiting_visible: Option<String>,
    pub mic: crate::audio::MicPermission,
    pub transcriber: ServiceState,
    pub tts: ServiceState,
    pub notice: Option<Notice>,
}

impl std::fmt::Debug for ConversationView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConversationView")
            .field("activity", &self.activity)
            .field("live_bytes", &self.live.len())
            .field("draft_bytes", &self.draft.as_ref().map(String::len))
            .field("turns", &self.turns)
            .field("speaking", &self.speaking)
            .field("awaiting_visible", &self.awaiting_visible)
            .field("mic", &self.mic)
            .field("transcriber", &self.transcriber)
            .field("tts", &self.tts)
            .field("notice", &self.notice)
            .finish()
    }
}

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
    /// that was cancelled mid-frame and the next one: the partial request is kept and completed
    /// by the next `recv` (contract clause `recv_is_cancel_safe_around_pushes`).
    fn send(&mut self, resp: IpcResponse) -> impl Future<Output = Result<(), IpcError>> + Send;
}

pub trait IpcServer: Send + Sync + 'static {
    type Conn: IpcConn;
    fn capabilities(&self) -> IpcCaps;
    /// Next authenticated connection. **Cancel-safe.**
    fn accept(&self) -> impl Future<Output = Result<(PeerIdentity, Self::Conn), IpcError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_code_accepts_what_people_paste() {
        for ok in ["482913", "482 913", "482-913", "482\u{a0}913", " 482913\t"] {
            assert_eq!(
                PairCode::parse(ok)
                    .map(|c| c.digits().to_owned())
                    .as_deref(),
                Some("482913"),
                "{ok:?}"
            );
        }
        for bad in ["48291", "4829134", "48a913", "", "４８２９１３"] {
            assert!(PairCode::parse(bad).is_none(), "{bad:?}");
        }
        assert!(!format!("{:?}", PairCode::parse("482913")).contains("482913"));
    }
}
