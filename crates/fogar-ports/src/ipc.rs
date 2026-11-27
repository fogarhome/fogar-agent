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

/// Largest password accepted (UTF-8 bytes; the gateway caps it at 1024).
pub const PASSWORD_MAX_BYTES: usize = 1024;

/// The person's account password, exactly as typed (never trimmed). It crosses the IPC once,
/// for `Login`: redacted `Debug`, wiped on drop, never stored.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(secrecy::zeroize::Zeroizing<String>);

impl Password {
    /// Keeps the password only if it is 1..=`PASSWORD_MAX_BYTES` bytes.
    pub fn parse(typed: &str) -> Option<Self> {
        (!typed.is_empty() && typed.len() <= PASSWORD_MAX_BYTES)
            .then(|| Self(secrecy::zeroize::Zeroizing::new(typed.to_owned())))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

/// What the person typed to sign in. `password` is `None` when it is empty or over
/// `PASSWORD_MAX_BYTES`: the core answers `BadCredentialsInput` without calling the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginForm {
    pub address: String,
    pub username: String,
    pub password: Option<Password>,
}

/// Requests a UI may send. Closed set (no generic "exec"). The address in `Login` is the person's
/// raw input; the core validates it (`GatewayAddr::parse`).
///
/// Authorization (IPC-4): `Login` and `Logout` change where the agent sends the user's voice and
/// under whose account, and `Reconnect` makes the agent spend the stored key to take the conversation back from another
/// session of the user, so the core answers them only for a `PeerCheck::SameSigner` peer; a
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
    /// Sign in to a gateway with the person's account (v5). Answered with `LoggedIn` or
    /// `LoginFailed`. Boxed so the enum (and every queue slot carrying it) stays its v4 size.
    Login(Box<LoginForm>),
    /// Sign out: tell the gateway (best effort), delete the account from the secret store and
    /// stop the link. Answered with `LoggedOut`.
    Logout,
    /// The person asks to connect again after the link parked for a reason only they can settle
    /// (gateway close 4011 "missing scope" or 4020/4021 agent refused once the admin fixed it,
    /// 4003 once the gateway was updated). Answered with `Reconnecting` (one new attempt; progress arrives as status
    /// `Event`s), or `Refused` when the link is not parked for such a reason.
    Reconnect,
    // ---- conversation (v4). Strictly `PeerCheck::SameSigner`: they carry or reveal user
    // content and can open the microphone. NOT `may_pair()`: the development uid-only opt-in
    // for signing in never extends to these. ----
    /// From now on the agent pushes `IpcResponse::Conversation` (latest view only). Answered
    /// with `ConversationSubscribed` + one `Conversation`. Only a subscribed connection may
    /// start a take or confirm a turn as visible.
    SubscribeConversation,
    /// Typed (or reviewed) text. Answered with `TextAccepted` or `TextRefused`.
    SendText { text: UserText },
    /// The person pressed the microphone button. The core's single consent point decides.
    /// Answered with `CaptureStarted` or `CaptureRefused`. The take belongs to the connection
    /// that opened it: if that connection closes or stops being subscribed (including being
    /// dropped for missing a push deadline), the take ends with `Discard`.
    ///
    /// Ordering (issue #26): before it answers `CaptureStarted`, the core publishes the view
    /// with `activity = UserActivity::Recording` (every later `Conversation` push carries it
    /// until the take ends). The app treats a view with any other activity received after
    /// `CaptureStarted` as "take closed", so a push of the previous view arriving after the answer would close the
    /// take with the microphone still open. This is a core obligation (the IPC adapter only
    /// frames what the core sends, in order); it is tested in fogar-core, not in the
    /// `ipc_server` contract suite.
    CaptureStart { gesture: CaptureGesture },
    /// The person ended the take. Answered with `CaptureEnded`.
    CaptureEnd { then: CaptureThen },
    /// Cancel button. Answered with `TurnCancelSent`.
    CancelTurn,
    // ---- the conversations (v6). Same-signer only, like the conversation above. ----
    /// From now on the agent pushes `IpcResponse::Conversations` (latest list only). Answered
    /// with `ListSubscribed` + one `Conversations`.
    ListConversations,
    /// Show this conversation (an id from the list). Answered with `ConversationOpened`; what it
    /// holds arrives in the next `Conversation` pushes.
    OpenConversation { id: String },
    /// Nothing open: the next `SendText` starts a new conversation. Answered with
    /// `ConversationOpened`.
    NewConversation,
    /// The person asked to allow the microphone (may show the system prompt). Answered with
    /// `MicPermission` once decided.
    RequestMicPermission,
}

/// Link state as the UI shows it. Mirrors fogar-core's link states without their internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkPhase {
    /// No account (wire value kept from S1): the UI shows the sign-in screen.
    NotPaired,
    Connecting,
    Ready,
    /// Waiting before the next attempt.
    Reconnecting,
    /// The gateway refused us in a way retrying on our own will not fix (bad config, client bug,
    /// missing scope). Some reasons accept a `Reconnect`.
    Parked,
    Stopping,
}

/// The gateway the agent is signed in to. Never carries the token.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GatewayInfo {
    /// `http(s)://host:port` form of the address.
    pub address: String,
    /// The home's name (the gateway's `name`, e.g. "green-house").
    pub name: String,
    /// Gateway version from the last `/.well-known` or `ready`; empty when unknown.
    pub version: String,
    /// The name this client was registered with (the admin may have overridden ours).
    pub client_name: String,
    /// The signed-in person's display name; empty when unknown.
    pub user: String,
}

impl GatewayInfo {
    pub fn new(address: String, name: String, version: String, client_name: String) -> Self {
        Self {
            address,
            name,
            version,
            client_name,
            user: String::new(),
        }
    }
    pub fn with_user(mut self, user: String) -> Self {
        self.user = user;
        self
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
    /// `None` when not signed in. Boxed so pushed status events stay small in every queue.
    pub gateway: Option<Box<GatewayInfo>>,
    pub sessions: u64,
    pub turns_completed: u64,
    pub boot_mode: String,
    pub peer_check: PeerCheck,
    /// The gateway's transcriber and TTS as last heard (`Unknown` until a session is ready).
    pub transcriber: ServiceState,
    pub tts: ServiceState,
    /// The account lacks `conversations:write` (`/auth/me`, or a `403 missing_scope`).
    pub read_only: bool,
}

impl StatusReport {
    pub fn with_voice(mut self, transcriber: ServiceState, tts: ServiceState) -> Self {
        self.transcriber = transcriber;
        self.tts = tts;
        self
    }
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
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
            gateway: gateway.map(Box::new),
            sessions,
            turns_completed,
            boot_mode,
            peer_check,
            transcriber: ServiceState::Unknown,
            tts: ServiceState::Unknown,
            read_only: false,
        }
    }
}

/// A gateway found by `Discover`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GatewayCandidate {
    /// `http(s)://host:port`, ready to send back in `Login`.
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

/// Why signing in did not happen. Each maps to one sentence in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoginFailure {
    /// The address does not parse.
    BadAddress,
    /// The password would travel in clear text over the home network: use `https://`, a
    /// `.ts.net` name or a tailnet address (option B). Nothing was sent.
    NeedsSecureAddress,
    /// Username or password empty or too long (never sent to the gateway).
    BadCredentialsInput,
    /// Nothing answered at that address.
    Unreachable,
    /// Something answered, but it is not a Fogar gateway.
    NotAGateway,
    /// 401: wrong username or password.
    InvalidCredentials,
    /// 403 `insecure_transport`: the gateway refuses passwords over this path.
    InsecureTransport,
    /// 429: too many attempts; wait and retry.
    TooManyAttempts,
    /// 5xx, an unexpected answer or none after sending.
    GatewayError,
    /// Signed in, but the account could not be stored.
    StorageFailed,
    /// Already signed in: sign out first.
    AlreadySignedIn,
    /// Another sign-in is in progress.
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
    LoggedIn(Box<GatewayInfo>),
    LoginFailed(LoginFailure),
    /// The account is gone from this machine (always, even when the gateway did not answer).
    /// `gateway_reached` = the gateway answered the logout.
    LoggedOut {
        gateway_reached: bool,
    },
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
    /// `CancelTurn` handled: `cancel` sent if a turn of the open conversation may run.
    TurnCancelSent,
    /// `ListConversations` accepted; one `Conversations` follows, then one per change.
    ListSubscribed,
    /// Pushed WITHOUT a request after `ListConversations`: the latest list (watch, no backlog).
    Conversations(Vec<ConversationSummary>),
    /// `OpenConversation` / `NewConversation` handled.
    ConversationOpened,
    /// Answer to `RequestMicPermission` (after the system prompt, if one was shown).
    MicPermission(crate::audio::MicPermission),
}

/// IPC protocol version spoken by this build. 2 = S1 (subscribe, discover, pair, unpair);
/// 3 = reconnect; 4 = conversation (S2); 5 = account (login/logout, `GatewayInfo::user`;
/// pair/unpair removed); 6 = the conversations over `/events` (list, open, new; messages and
/// turns as separate entries; `TurnVisible`, `speaking`, `awaiting_visible` and the view's
/// `tts` removed).
pub const IPC_VERSION: u32 = 6;

// ---- conversation (IPC v4, spec S2) ------------------------------------------------------
//
// Everything below carries the user's content. The core answers these requests only for a
// `PeerCheck::SameSigner` peer (like Login), never logs the text, and every type here prints
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
    /// The account may read but not write conversations (`conversations:write` missing): the
    /// administrator has to grant it. Never retried on its own.
    MissingScope,
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

/// One turn of the agent (v6: the person's message is its own [`MessageView`]). Built by the
/// core; fields are public so it can fill a `default()` value (the struct is `non_exhaustive`).
#[derive(Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct TurnView {
    /// Empty for a turn from history.
    pub turn_id: String,
    pub source: TurnSource,
    /// The device that started it (a person's turn), as the gateway names it.
    pub by_name: Option<String>,
    /// Started from this device.
    pub by_self: bool,
    /// Fogar's reply so far.
    pub reply: String,
    /// Current progress while no text is arriving; `None` once tokens flow or the turn ended.
    pub progress: Option<ProgressKind>,
    pub tool: Option<String>,
    /// `None` while the turn is open.
    pub outcome: Option<TurnOutcome>,
    /// The gateway's stable code when `outcome` is `Error`.
    pub error_code: Option<String>,
}

impl std::fmt::Debug for TurnView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnView")
            .field("turn_id", &self.turn_id)
            .field("source", &self.source)
            .field("by_name_bytes", &self.by_name.as_ref().map(String::len))
            .field("by_self", &self.by_self)
            .field("reply_bytes", &self.reply.len())
            .field("progress", &self.progress)
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

/// Where a person's message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum MessageState {
    /// Waiting behind a running turn.
    Pending,
    /// Reached the agent.
    #[default]
    Delivered,
    /// From history.
    Past,
    /// Our send failed (`MessageView::error_code` says why): it never reached the gateway.
    Failed,
}

/// A person's message (this device's or another's).
#[derive(Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct MessageView {
    /// The gateway's id of a live message; empty for history and failures.
    pub id: String,
    pub text: String,
    /// Sent from this device.
    pub mine: bool,
    /// The sending device's name when another device sent it.
    pub from_name: Option<String>,
    pub state: MessageState,
    pub error_code: Option<String>,
}

impl std::fmt::Debug for MessageView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageView")
            .field("id", &self.id)
            .field("text_bytes", &self.text.len())
            .field("mine", &self.mine)
            .field("from_name_bytes", &self.from_name.as_ref().map(String::len))
            .field("state", &self.state)
            .field("error_code", &self.error_code)
            .finish()
    }
}

/// One line of the conversation, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ViewEntry {
    Message(MessageView),
    Turn(TurnView),
}

/// One conversation of the list. `Debug` hides the title (derived from user text).
#[derive(Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ConversationSummary {
    pub id: String,
    pub title: Option<String>,
    /// Private to one person, else the home's.
    pub private: bool,
    /// As the gateway gave it (ISO 8601); `None` when it never had a turn.
    pub last_turn_at: Option<String>,
    /// A turn runs in it now (any device): «en marcha».
    pub running: bool,
}

impl std::fmt::Debug for ConversationSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConversationSummary")
            .field("id", &self.id)
            .field("title_bytes", &self.title.as_ref().map(String::len))
            .field("private", &self.private)
            .field("running", &self.running)
            .finish_non_exhaustive()
    }
}

/// The conversation as the UI draws it: a bounded snapshot pushed latest-only (Decision 3 of
/// spec S2). The UI keeps nothing of its own.
#[derive(Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ConversationView {
    /// The open conversation; `None` = nothing open (the next text starts one).
    pub id: Option<String>,
    pub activity: UserActivity,
    /// The live `transcription_partial` while recording.
    pub live: String,
    /// The transcript waiting for review in the text box.
    pub draft: Option<String>,
    /// Oldest first, bounded by `CONVERSATION_MAX_TURNS` exchanges and
    /// `CONVERSATION_MAX_TEXT_BYTES` of text.
    pub entries: Vec<ViewEntry>,
    pub mic: crate::audio::MicPermission,
    pub transcriber: ServiceState,
    pub notice: Option<Notice>,
    /// The account lacks `conversations:write`: it can read, not send.
    pub read_only: bool,
}

impl std::fmt::Debug for ConversationView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConversationView")
            .field("id", &self.id)
            .field("activity", &self.activity)
            .field("live_bytes", &self.live.len())
            .field("draft_bytes", &self.draft.as_ref().map(String::len))
            .field("entries", &self.entries)
            .field("mic", &self.mic)
            .field("transcriber", &self.transcriber)
            .field("notice", &self.notice)
            .field("read_only", &self.read_only)
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
    fn password_parse_bounds_and_redacts() {
        assert!(Password::parse("").is_none());
        assert!(Password::parse(&"a".repeat(PASSWORD_MAX_BYTES + 1)).is_none());
        // Bytes, not characters: 513 two-byte chars are over the cap.
        assert!(Password::parse(&"ñ".repeat(513)).is_none());
        for ok in [
            " ",
            " espacios \"y\" \\ ",
            "x",
            &"a".repeat(PASSWORD_MAX_BYTES),
        ] {
            let p = Password::parse(ok).unwrap_or_else(|| panic!("{ok:?}"));
            assert_eq!(p.expose(), ok, "never trimmed or altered");
        }
        let p = Password::parse("hunter2-secreta").unwrap();
        assert_eq!(format!("{p:?}"), "Password(<redacted>)");
        let r = IpcRequest::Login(Box::new(LoginForm {
            address: "https://casa".into(),
            username: "sito".into(),
            password: Some(p),
        }));
        assert!(!format!("{r:?}").contains("hunter2"));
    }
}
