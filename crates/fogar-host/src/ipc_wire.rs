//! The local UI wire format (ADR-0002, ADR-0004), shared by every OS's IPC adapter and the single
//! source for the Swift types (typeshare, S1): 4-byte big-endian length + one JSON object, at most
//! `MAX_FRAME_BYTES`, `"type"`-tagged, `deny_unknown_fields`, closed vocabulary.
//!
//! Sans-I/O: framing works on byte buffers; adapters own the socket and peer authentication.
//! Pairing codes cross this layer once; the frame buffers holding them are wiped. serde_json's
//! internal scratch for escaped strings is not (codes are 6 digits, never escaped). No key
//! ever crosses IPC.

use fogar_ports::{
    audio::MicPermission,
    ipc::{
        CaptureGesture, CaptureRefusal, CaptureThen, ConversationView, GatewayCandidate,
        GatewayInfo, IpcError, IpcRequest, IpcResponse, LinkPhase, MAX_FRAME_BYTES, Notice,
        PairCode, PairFailure, PeerCheck, ProgressKind, ServiceState, StatusReport, TextRefusal,
        TurnAudio, TurnOutcome, TurnSource, TurnView, UserActivity, UserText,
    },
};
use secrecy::zeroize::{Zeroize, Zeroizing};
use serde::{Deserialize, Serialize};

/// The pairing code on the wire: redacted `Debug`, wiped on drop.
#[cfg_attr(typeshare, typeshare)]
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct WireCode(String);

impl std::fmt::Debug for WireCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Drop for WireCode {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

// Shape rules (so `typeshare --lang=swift` can generate `Codable` types, ADR-0004): enums
// that carry data are adjacently tagged `{"type": ..., "data": {...}}` with one struct per
// variant; data-less variants are `{"type": ...}`; counters are `u64` sent as Swift `Int`.
// Changing any of this, or adding an enum value, is an IPC_VERSION bump (the Swift enums are
// closed). The golden JSON test below pins the shape.

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireHello {
    pub version: u32,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WirePair {
    pub address: String,
    pub code: WireCode,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WireRequest {
    Hello(WireHello),
    Status,
    Subscribe,
    Discover,
    Pair(WirePair),
    Unpair,
    Reconnect,
    SubscribeConversation,
    SendText(WireSendText),
    CaptureStart(WireCaptureStart),
    CaptureEnd(WireCaptureEnd),
    CancelTurn,
    TurnVisible(WireTurnVisible),
    RequestMicPermission,
}

// ---- conversation (v4): user content. Redacted `Debug` on everything that holds text. ----

#[cfg_attr(typeshare, typeshare)]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireSendText {
    pub text: String,
}

impl std::fmt::Debug for WireSendText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WireSendText({} B)", self.text.len())
    }
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireGesture {
    Hold,
    Toggle,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireCaptureStart {
    pub gesture: WireGesture,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireThen {
    Send,
    Review,
    Discard,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireCaptureEnd {
    pub then: WireThen,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireTurnVisible {
    pub turn_id: String,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireCaptureRefusal {
    NotSubscribed,
    PermissionNotDetermined,
    PermissionDenied,
    NoMicrophone,
    NotReady,
    NoTranscriber,
    Busy,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireCaptureRefused {
    pub reason: WireCaptureRefusal,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireTextRefusal {
    Empty,
    TooLong,
    NotReady,
    Recording,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireTextRefused {
    pub reason: WireTextRefusal,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireMic {
    Granted,
    Denied,
    NotDetermined,
    Restricted,
    Unknown,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireMicPermission {
    pub state: WireMic,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireService {
    Unknown,
    Up,
    Degraded,
    Down,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireActivity {
    Idle,
    Recording,
    Transcribing,
    AwaitingReply,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireNotice {
    PhraseLost,
    SilenceCut,
    SafetyCut,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireSource {
    Mine,
    Push,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireProgress {
    Preparing,
    Thinking,
    Tool,
    Working,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireOutcome {
    Done,
    Cancelled,
    Interrupted,
    Replaced,
    Timeout,
    Error,
    Closed,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireAudio {
    None,
    Played,
    Muted,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireTurn {
    pub turn_id: String,
    pub conversation: Option<String>,
    pub source: WireSource,
    pub mine: String,
    pub reply: String,
    pub progress: Option<WireProgress>,
    pub tool: Option<String>,
    pub outcome: Option<WireOutcome>,
    pub error_code: Option<String>,
    pub audio: WireAudio,
}

impl std::fmt::Debug for WireTurn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WireTurn({}, mine {} B, reply {} B)",
            self.turn_id,
            self.mine.len(),
            self.reply.len()
        )
    }
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireConversation {
    pub activity: WireActivity,
    pub live: String,
    pub draft: Option<String>,
    pub turns: Vec<WireTurn>,
    pub speaking: bool,
    pub awaiting_visible: Option<String>,
    pub mic: WireMic,
    pub transcriber: WireService,
    pub tts: WireService,
    pub notice: Option<WireNotice>,
}

impl std::fmt::Debug for WireConversation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WireConversation({:?}, {} turns, live {} B)",
            self.activity,
            self.turns.len(),
            self.live.len()
        )
    }
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WirePhase {
    NotPaired,
    Connecting,
    Ready,
    Reconnecting,
    Parked,
    Stopping,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WirePeerCheck {
    UidOnly,
    SameSigner,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WirePairFailure {
    BadAddress,
    BadCode,
    Unreachable,
    NotAGateway,
    InvalidCode,
    GatewayError,
    StorageFailed,
    AlreadyPaired,
    Busy,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireGateway {
    pub address: String,
    pub name: String,
    pub version: String,
    pub client_name: String,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireCandidate {
    pub address: String,
    pub name: String,
    pub version: String,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireStatus {
    pub link: String,
    pub phase: WirePhase,
    pub gateway: Option<WireGateway>,
    #[cfg_attr(typeshare, typeshare(serialized_as = "Int"))]
    pub sessions: u64,
    #[cfg_attr(typeshare, typeshare(serialized_as = "Int"))]
    pub turns_completed: u64,
    pub boot_mode: String,
    pub peer_check: WirePeerCheck,
    pub transcriber: WireService,
    pub tts: WireService,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireRefused {
    pub reason: String,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireDiscovered {
    pub gateways: Vec<WireCandidate>,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WirePairFailed {
    pub reason: WirePairFailure,
}

#[cfg_attr(typeshare, typeshare)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WireResponse {
    Hello(WireHello),
    Status(WireStatus),
    Refused(WireRefused),
    Subscribed,
    Event(WireStatus),
    Discovered(WireDiscovered),
    Paired(WireGateway),
    PairFailed(WirePairFailed),
    Unpaired,
    Reconnecting,
    ConversationSubscribed,
    Conversation(WireConversation),
    CaptureStarted,
    CaptureRefused(WireCaptureRefused),
    CaptureEnded,
    TextAccepted,
    TextRefused(WireTextRefused),
    TurnCancelSent,
    TurnVisibleNoted,
    MicPermission(WireMicPermission),
}

// ---- framing -----------------------------------------------------------------------------

/// Pops one complete frame body from `buf` if present (leaves partial frames in place, so a
/// reader that keeps `buf` across cancelled reads is cancel-safe).
/// The returned body and the consumed bytes of `buf` are wiped (a pairing code may be inside).
pub fn take_frame(buf: &mut Vec<u8>) -> Result<Option<Zeroizing<Vec<u8>>>, IpcError> {
    let Some(hdr) = buf.first_chunk::<4>() else {
        return Ok(None);
    };
    let len = usize::try_from(u32::from_be_bytes(*hdr)).unwrap_or(usize::MAX);
    if len > MAX_FRAME_BYTES {
        return Err(IpcError::TooLarge {
            len,
            max: MAX_FRAME_BYTES,
        });
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let body = Zeroizing::new(buf[4..4 + len].to_vec());
    buf[..4 + len].zeroize();
    buf.drain(..4 + len);
    Ok(Some(body))
}

/// Length-prefixes `body` into one pre-sized, wiped-on-drop buffer.
pub fn frame(body: &[u8]) -> Result<Zeroizing<Vec<u8>>, IpcError> {
    if body.len() > MAX_FRAME_BYTES {
        return Err(IpcError::TooLarge {
            len: body.len(),
            max: MAX_FRAME_BYTES,
        });
    }
    let len = u32::try_from(body.len()).map_err(|_| IpcError::Malformed)?;
    let mut out = Zeroizing::new(Vec::with_capacity(4 + body.len()));
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

// ---- requests ----------------------------------------------------------------------------

/// Server side: frame body -> port request. Anything outside the vocabulary is `Malformed`.
pub fn decode_request(body: &[u8]) -> Result<IpcRequest, IpcError> {
    Ok(
        match serde_json::from_slice::<WireRequest>(body).map_err(|_| IpcError::Malformed)? {
            WireRequest::Hello(h) => IpcRequest::Hello { version: h.version },
            WireRequest::Status => IpcRequest::Status,
            WireRequest::Subscribe => IpcRequest::Subscribe,
            WireRequest::Discover => IpcRequest::Discover,
            WireRequest::Pair(p) => IpcRequest::Pair {
                code: PairCode::parse(&p.code.0),
                address: p.address,
            },
            WireRequest::Unpair => IpcRequest::Unpair,
            WireRequest::Reconnect => IpcRequest::Reconnect,
            WireRequest::SubscribeConversation => IpcRequest::SubscribeConversation,
            WireRequest::SendText(t) => IpcRequest::SendText {
                text: UserText(t.text),
            },
            WireRequest::CaptureStart(c) => IpcRequest::CaptureStart {
                gesture: match c.gesture {
                    WireGesture::Hold => CaptureGesture::Hold,
                    WireGesture::Toggle => CaptureGesture::Toggle,
                },
            },
            WireRequest::CaptureEnd(c) => IpcRequest::CaptureEnd {
                then: match c.then {
                    WireThen::Send => CaptureThen::Send,
                    WireThen::Review => CaptureThen::Review,
                    WireThen::Discard => CaptureThen::Discard,
                },
            },
            WireRequest::CancelTurn => IpcRequest::CancelTurn,
            WireRequest::TurnVisible(v) => IpcRequest::TurnVisible { turn_id: v.turn_id },
            WireRequest::RequestMicPermission => IpcRequest::RequestMicPermission,
        },
    )
}

/// Client side (CLI, contract tests, the reference for the Swift app).
pub fn encode_request(r: &IpcRequest) -> Result<Zeroizing<Vec<u8>>, IpcError> {
    let w = match r {
        IpcRequest::Hello { version } => WireRequest::Hello(WireHello { version: *version }),
        IpcRequest::Status => WireRequest::Status,
        IpcRequest::Subscribe => WireRequest::Subscribe,
        IpcRequest::Discover => WireRequest::Discover,
        IpcRequest::Pair { address, code } => WireRequest::Pair(WirePair {
            address: address.clone(),
            code: WireCode(
                code.as_ref()
                    .map(|c| c.digits().to_owned())
                    .unwrap_or_default(),
            ),
        }),
        IpcRequest::Unpair => WireRequest::Unpair,
        IpcRequest::Reconnect => WireRequest::Reconnect,
        IpcRequest::SubscribeConversation => WireRequest::SubscribeConversation,
        IpcRequest::SendText { text } => WireRequest::SendText(WireSendText {
            text: text.0.clone(),
        }),
        IpcRequest::CaptureStart { gesture } => WireRequest::CaptureStart(WireCaptureStart {
            gesture: match gesture {
                CaptureGesture::Hold => WireGesture::Hold,
                _ => WireGesture::Toggle,
            },
        }),
        IpcRequest::CaptureEnd { then } => WireRequest::CaptureEnd(WireCaptureEnd {
            then: match then {
                CaptureThen::Send => WireThen::Send,
                CaptureThen::Review => WireThen::Review,
                // Discard and any future ending: the safe one (nothing is sent).
                _ => WireThen::Discard,
            },
        }),
        IpcRequest::CancelTurn => WireRequest::CancelTurn,
        IpcRequest::TurnVisible { turn_id } => WireRequest::TurnVisible(WireTurnVisible {
            turn_id: turn_id.clone(),
        }),
        IpcRequest::RequestMicPermission => WireRequest::RequestMicPermission,
        _ => return Err(IpcError::Unsupported("request not in the wire vocabulary")),
    };
    serde_json::to_vec(&w)
        .map(Zeroizing::new)
        .map_err(|_| IpcError::Malformed)
}

// ---- responses ---------------------------------------------------------------------------

fn phase_to_wire(p: LinkPhase) -> WirePhase {
    match p {
        LinkPhase::NotPaired => WirePhase::NotPaired,
        LinkPhase::Connecting => WirePhase::Connecting,
        LinkPhase::Ready => WirePhase::Ready,
        LinkPhase::Reconnecting => WirePhase::Reconnecting,
        LinkPhase::Parked => WirePhase::Parked,
        // Stopping and any future phase.
        _ => WirePhase::Stopping,
    }
}

fn phase_from_wire(p: WirePhase) -> LinkPhase {
    match p {
        WirePhase::NotPaired => LinkPhase::NotPaired,
        WirePhase::Connecting => LinkPhase::Connecting,
        WirePhase::Ready => LinkPhase::Ready,
        WirePhase::Reconnecting => LinkPhase::Reconnecting,
        WirePhase::Parked => LinkPhase::Parked,
        WirePhase::Stopping => LinkPhase::Stopping,
    }
}

fn failure_to_wire(f: PairFailure) -> WirePairFailure {
    match f {
        PairFailure::BadAddress => WirePairFailure::BadAddress,
        PairFailure::BadCode => WirePairFailure::BadCode,
        PairFailure::Unreachable => WirePairFailure::Unreachable,
        PairFailure::NotAGateway => WirePairFailure::NotAGateway,
        PairFailure::InvalidCode => WirePairFailure::InvalidCode,
        PairFailure::StorageFailed => WirePairFailure::StorageFailed,
        PairFailure::AlreadyPaired => WirePairFailure::AlreadyPaired,
        PairFailure::Busy => WirePairFailure::Busy,
        // GatewayError and any future failure: "try again".
        _ => WirePairFailure::GatewayError,
    }
}

fn failure_from_wire(f: WirePairFailure) -> PairFailure {
    match f {
        WirePairFailure::BadAddress => PairFailure::BadAddress,
        WirePairFailure::BadCode => PairFailure::BadCode,
        WirePairFailure::Unreachable => PairFailure::Unreachable,
        WirePairFailure::NotAGateway => PairFailure::NotAGateway,
        WirePairFailure::InvalidCode => PairFailure::InvalidCode,
        WirePairFailure::GatewayError => PairFailure::GatewayError,
        WirePairFailure::StorageFailed => PairFailure::StorageFailed,
        WirePairFailure::AlreadyPaired => PairFailure::AlreadyPaired,
        WirePairFailure::Busy => PairFailure::Busy,
    }
}

fn gateway_to_wire(g: GatewayInfo) -> WireGateway {
    WireGateway {
        address: g.address,
        name: g.name,
        version: g.version,
        client_name: g.client_name,
    }
}

fn gateway_from_wire(g: WireGateway) -> GatewayInfo {
    GatewayInfo::new(g.address, g.name, g.version, g.client_name)
}

fn service_to_wire(s: ServiceState) -> WireService {
    match s {
        ServiceState::Up => WireService::Up,
        ServiceState::Degraded => WireService::Degraded,
        ServiceState::Down => WireService::Down,
        _ => WireService::Unknown,
    }
}

fn service_from_wire(s: WireService) -> ServiceState {
    match s {
        WireService::Unknown => ServiceState::Unknown,
        WireService::Up => ServiceState::Up,
        WireService::Degraded => ServiceState::Degraded,
        WireService::Down => ServiceState::Down,
    }
}

fn mic_to_wire(m: MicPermission) -> WireMic {
    match m {
        MicPermission::Granted => WireMic::Granted,
        MicPermission::Denied => WireMic::Denied,
        MicPermission::NotDetermined => WireMic::NotDetermined,
        MicPermission::Restricted => WireMic::Restricted,
        _ => WireMic::Unknown,
    }
}

fn mic_from_wire(m: WireMic) -> MicPermission {
    match m {
        WireMic::Granted => MicPermission::Granted,
        WireMic::Denied => MicPermission::Denied,
        WireMic::NotDetermined => MicPermission::NotDetermined,
        WireMic::Restricted => MicPermission::Restricted,
        WireMic::Unknown => MicPermission::Unknown,
    }
}

fn capture_refusal_to_wire(r: CaptureRefusal) -> WireCaptureRefusal {
    match r {
        CaptureRefusal::NotSubscribed => WireCaptureRefusal::NotSubscribed,
        CaptureRefusal::PermissionNotDetermined => WireCaptureRefusal::PermissionNotDetermined,
        CaptureRefusal::PermissionDenied => WireCaptureRefusal::PermissionDenied,
        CaptureRefusal::NoMicrophone => WireCaptureRefusal::NoMicrophone,
        CaptureRefusal::NotReady => WireCaptureRefusal::NotReady,
        CaptureRefusal::NoTranscriber => WireCaptureRefusal::NoTranscriber,
        _ => WireCaptureRefusal::Busy,
    }
}

fn capture_refusal_from_wire(r: WireCaptureRefusal) -> CaptureRefusal {
    match r {
        WireCaptureRefusal::NotSubscribed => CaptureRefusal::NotSubscribed,
        WireCaptureRefusal::PermissionNotDetermined => CaptureRefusal::PermissionNotDetermined,
        WireCaptureRefusal::PermissionDenied => CaptureRefusal::PermissionDenied,
        WireCaptureRefusal::NoMicrophone => CaptureRefusal::NoMicrophone,
        WireCaptureRefusal::NotReady => CaptureRefusal::NotReady,
        WireCaptureRefusal::NoTranscriber => CaptureRefusal::NoTranscriber,
        WireCaptureRefusal::Busy => CaptureRefusal::Busy,
    }
}

fn text_refusal_to_wire(r: TextRefusal) -> WireTextRefusal {
    match r {
        TextRefusal::Empty => WireTextRefusal::Empty,
        TextRefusal::TooLong => WireTextRefusal::TooLong,
        TextRefusal::Recording => WireTextRefusal::Recording,
        _ => WireTextRefusal::NotReady,
    }
}

fn text_refusal_from_wire(r: WireTextRefusal) -> TextRefusal {
    match r {
        WireTextRefusal::Empty => TextRefusal::Empty,
        WireTextRefusal::TooLong => TextRefusal::TooLong,
        WireTextRefusal::NotReady => TextRefusal::NotReady,
        WireTextRefusal::Recording => TextRefusal::Recording,
    }
}

fn turn_to_wire(t: TurnView) -> WireTurn {
    WireTurn {
        turn_id: t.turn_id,
        conversation: t.conversation,
        source: match t.source {
            TurnSource::Push => WireSource::Push,
            _ => WireSource::Mine,
        },
        mine: t.mine,
        reply: t.reply,
        progress: t.progress.map(|p| match p {
            ProgressKind::Preparing => WireProgress::Preparing,
            ProgressKind::Thinking => WireProgress::Thinking,
            ProgressKind::Tool => WireProgress::Tool,
            _ => WireProgress::Working,
        }),
        tool: t.tool,
        outcome: t.outcome.map(|o| match o {
            TurnOutcome::Done => WireOutcome::Done,
            TurnOutcome::Cancelled => WireOutcome::Cancelled,
            TurnOutcome::Interrupted => WireOutcome::Interrupted,
            TurnOutcome::Replaced => WireOutcome::Replaced,
            TurnOutcome::Timeout => WireOutcome::Timeout,
            TurnOutcome::Closed => WireOutcome::Closed,
            _ => WireOutcome::Error,
        }),
        error_code: t.error_code,
        audio: match t.audio {
            TurnAudio::Played => WireAudio::Played,
            TurnAudio::Muted => WireAudio::Muted,
            _ => WireAudio::None,
        },
    }
}

fn turn_from_wire(t: WireTurn) -> TurnView {
    let mut v = TurnView::default();
    v.turn_id = t.turn_id;
    v.conversation = t.conversation;
    v.source = match t.source {
        WireSource::Mine => TurnSource::Mine,
        WireSource::Push => TurnSource::Push,
    };
    v.mine = t.mine;
    v.reply = t.reply;
    v.progress = t.progress.map(|p| match p {
        WireProgress::Preparing => ProgressKind::Preparing,
        WireProgress::Thinking => ProgressKind::Thinking,
        WireProgress::Tool => ProgressKind::Tool,
        WireProgress::Working => ProgressKind::Working,
    });
    v.tool = t.tool;
    v.outcome = t.outcome.map(|o| match o {
        WireOutcome::Done => TurnOutcome::Done,
        WireOutcome::Cancelled => TurnOutcome::Cancelled,
        WireOutcome::Interrupted => TurnOutcome::Interrupted,
        WireOutcome::Replaced => TurnOutcome::Replaced,
        WireOutcome::Timeout => TurnOutcome::Timeout,
        WireOutcome::Error => TurnOutcome::Error,
        WireOutcome::Closed => TurnOutcome::Closed,
    });
    v.error_code = t.error_code;
    v.audio = match t.audio {
        WireAudio::None => TurnAudio::None,
        WireAudio::Played => TurnAudio::Played,
        WireAudio::Muted => TurnAudio::Muted,
    };
    v
}

fn conversation_to_wire(c: ConversationView) -> WireConversation {
    WireConversation {
        activity: match c.activity {
            UserActivity::Recording => WireActivity::Recording,
            UserActivity::Transcribing => WireActivity::Transcribing,
            UserActivity::AwaitingReply => WireActivity::AwaitingReply,
            _ => WireActivity::Idle,
        },
        live: c.live,
        draft: c.draft,
        turns: c.turns.into_iter().map(turn_to_wire).collect(),
        speaking: c.speaking,
        awaiting_visible: c.awaiting_visible,
        mic: mic_to_wire(c.mic),
        transcriber: service_to_wire(c.transcriber),
        tts: service_to_wire(c.tts),
        notice: c.notice.map(|n| match n {
            Notice::PhraseLost => WireNotice::PhraseLost,
            Notice::SilenceCut => WireNotice::SilenceCut,
            _ => WireNotice::SafetyCut,
        }),
    }
}

fn conversation_from_wire(c: WireConversation) -> ConversationView {
    let mut v = ConversationView::default();
    v.activity = match c.activity {
        WireActivity::Idle => UserActivity::Idle,
        WireActivity::Recording => UserActivity::Recording,
        WireActivity::Transcribing => UserActivity::Transcribing,
        WireActivity::AwaitingReply => UserActivity::AwaitingReply,
    };
    v.live = c.live;
    v.draft = c.draft;
    v.turns = c.turns.into_iter().map(turn_from_wire).collect();
    v.speaking = c.speaking;
    v.awaiting_visible = c.awaiting_visible;
    v.mic = mic_from_wire(c.mic);
    v.transcriber = service_from_wire(c.transcriber);
    v.tts = service_from_wire(c.tts);
    v.notice = c.notice.map(|n| match n {
        WireNotice::PhraseLost => Notice::PhraseLost,
        WireNotice::SilenceCut => Notice::SilenceCut,
        WireNotice::SafetyCut => Notice::SafetyCut,
    });
    v
}

/// Halves `s` at a char boundary (never empty -> never loops on an empty string).
fn halve(s: &mut String) {
    let mut cut = s.len() / 2;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

/// Encodes a conversation so the frame ALWAYS fits `MAX_FRAME_BYTES`, whatever the content
/// (JSON escaping can make text up to 6x larger): drops the oldest turns first, then halves the
/// longest text field. Lossy only in that extreme; the core keeps its own budget well below.
fn encode_conversation(mut c: WireConversation) -> Result<Vec<u8>, IpcError> {
    loop {
        let body = serde_json::to_vec(&WireResponse::Conversation(c.clone()))
            .map_err(|_| IpcError::Malformed)?;
        if body.len() <= MAX_FRAME_BYTES {
            return Ok(body);
        }
        if c.turns.len() > 1 {
            c.turns.remove(0);
            continue;
        }
        let mut fields: Vec<&mut String> = vec![&mut c.live];
        if let Some(d) = c.draft.as_mut() {
            fields.push(d);
        }
        for t in &mut c.turns {
            fields.push(&mut t.mine);
            fields.push(&mut t.reply);
        }
        if let Some(longest) = fields
            .into_iter()
            .max_by_key(|s| s.len())
            .filter(|s| !s.is_empty())
        {
            halve(longest);
            continue;
        }
        // Only gateway-controlled ids and codes left (unbounded up to the gateway's 1 MiB frame):
        // drop them rather than fail the push forever. A turn whose id is dropped stays muted:
        // the UI cannot confirm it (fails safe).
        if !c.turns.is_empty() {
            c.turns.clear();
        } else if c.awaiting_visible.is_some() {
            c.awaiting_visible = None;
        } else {
            return Err(IpcError::TooLarge {
                len: body.len(),
                max: MAX_FRAME_BYTES,
            });
        }
    }
}

fn status_to_wire(s: StatusReport) -> WireStatus {
    WireStatus {
        link: s.link,
        phase: phase_to_wire(s.phase),
        gateway: s.gateway.map(gateway_to_wire),
        sessions: s.sessions,
        turns_completed: s.turns_completed,
        boot_mode: s.boot_mode,
        peer_check: match s.peer_check {
            PeerCheck::SameSigner => WirePeerCheck::SameSigner,
            _ => WirePeerCheck::UidOnly,
        },
        transcriber: service_to_wire(s.transcriber),
        tts: service_to_wire(s.tts),
    }
}

fn status_from_wire(s: WireStatus) -> StatusReport {
    StatusReport::new(
        s.link,
        phase_from_wire(s.phase),
        s.gateway.map(gateway_from_wire),
        s.sessions,
        s.turns_completed,
        s.boot_mode,
        match s.peer_check {
            WirePeerCheck::SameSigner => PeerCheck::SameSigner,
            WirePeerCheck::UidOnly => PeerCheck::UidOnly,
        },
    )
    .with_voice(service_from_wire(s.transcriber), service_from_wire(s.tts))
}

/// Server side: port response -> frame body.
pub fn encode_response(r: IpcResponse) -> Result<Vec<u8>, IpcError> {
    let w = match r {
        IpcResponse::Hello { version } => WireResponse::Hello(WireHello { version }),
        IpcResponse::Status(s) => WireResponse::Status(status_to_wire(s)),
        IpcResponse::Refused { reason } => WireResponse::Refused(WireRefused {
            reason: reason.to_owned(),
        }),
        IpcResponse::Subscribed => WireResponse::Subscribed,
        IpcResponse::Event(s) => WireResponse::Event(status_to_wire(s)),
        IpcResponse::Discovered(c) => WireResponse::Discovered(WireDiscovered {
            gateways: c
                .into_iter()
                .map(|c| WireCandidate {
                    address: c.address,
                    name: c.name,
                    version: c.version,
                })
                .collect(),
        }),
        IpcResponse::Paired(g) => WireResponse::Paired(gateway_to_wire(g)),
        IpcResponse::PairFailed(f) => WireResponse::PairFailed(WirePairFailed {
            reason: failure_to_wire(f),
        }),
        IpcResponse::Unpaired => WireResponse::Unpaired,
        IpcResponse::Reconnecting => WireResponse::Reconnecting,
        IpcResponse::ConversationSubscribed => WireResponse::ConversationSubscribed,
        IpcResponse::Conversation(c) => return encode_conversation(conversation_to_wire(c)),
        IpcResponse::CaptureStarted => WireResponse::CaptureStarted,
        IpcResponse::CaptureRefused(r) => WireResponse::CaptureRefused(WireCaptureRefused {
            reason: capture_refusal_to_wire(r),
        }),
        IpcResponse::CaptureEnded => WireResponse::CaptureEnded,
        IpcResponse::TextAccepted => WireResponse::TextAccepted,
        IpcResponse::TextRefused(r) => WireResponse::TextRefused(WireTextRefused {
            reason: text_refusal_to_wire(r),
        }),
        IpcResponse::TurnCancelSent => WireResponse::TurnCancelSent,
        IpcResponse::TurnVisibleNoted => WireResponse::TurnVisibleNoted,
        IpcResponse::MicPermission(m) => WireResponse::MicPermission(WireMicPermission {
            state: mic_to_wire(m),
        }),
        _ => WireResponse::Refused(WireRefused {
            reason: "unsupported response".to_owned(),
        }),
    };
    serde_json::to_vec(&w).map_err(|_| IpcError::Malformed)
}

/// Client side: frame body -> port response. `Refused` loses its free-text reason (the port
/// type holds only static strings).
pub fn decode_response(body: &[u8]) -> Result<IpcResponse, IpcError> {
    Ok(
        match serde_json::from_slice::<WireResponse>(body).map_err(|_| IpcError::Malformed)? {
            WireResponse::Hello(h) => IpcResponse::Hello { version: h.version },
            WireResponse::Status(s) => IpcResponse::Status(status_from_wire(s)),
            WireResponse::Refused(_) => IpcResponse::Refused { reason: "refused" },
            WireResponse::Subscribed => IpcResponse::Subscribed,
            WireResponse::Event(s) => IpcResponse::Event(status_from_wire(s)),
            WireResponse::Discovered(d) => IpcResponse::Discovered(
                d.gateways
                    .into_iter()
                    .map(|c| GatewayCandidate::new(c.address, c.name, c.version))
                    .collect(),
            ),
            WireResponse::Paired(g) => IpcResponse::Paired(gateway_from_wire(g)),
            WireResponse::PairFailed(f) => IpcResponse::PairFailed(failure_from_wire(f.reason)),
            WireResponse::Unpaired => IpcResponse::Unpaired,
            WireResponse::Reconnecting => IpcResponse::Reconnecting,
            WireResponse::ConversationSubscribed => IpcResponse::ConversationSubscribed,
            WireResponse::Conversation(c) => IpcResponse::Conversation(conversation_from_wire(c)),
            WireResponse::CaptureStarted => IpcResponse::CaptureStarted,
            WireResponse::CaptureRefused(r) => {
                IpcResponse::CaptureRefused(capture_refusal_from_wire(r.reason))
            }
            WireResponse::CaptureEnded => IpcResponse::CaptureEnded,
            WireResponse::TextAccepted => IpcResponse::TextAccepted,
            WireResponse::TextRefused(r) => {
                IpcResponse::TextRefused(text_refusal_from_wire(r.reason))
            }
            WireResponse::TurnCancelSent => IpcResponse::TurnCancelSent,
            WireResponse::TurnVisibleNoted => IpcResponse::TurnVisibleNoted,
            WireResponse::MicPermission(m) => IpcResponse::MicPermission(mic_from_wire(m.state)),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> StatusReport {
        StatusReport::new(
            "ready".into(),
            LinkPhase::Ready,
            Some(GatewayInfo::new(
                "http://green-house:8004".into(),
                "green-house".into(),
                "1.29.0".into(),
                "Mac de Sito".into(),
            )),
            3,
            7,
            "normal".into(),
            PeerCheck::SameSigner,
        )
        .with_voice(ServiceState::Up, ServiceState::Down)
    }

    fn conversation() -> ConversationView {
        let mut t = TurnView::default();
        t.turn_id = "t-3".into();
        t.conversation = Some("0b9d".into());
        t.mine = "¿qué tiempo hace?".into();
        t.reply = "Soleado.".into();
        t.progress = Some(ProgressKind::Tool);
        t.tool = Some("ha_call_service".into());
        let mut push = TurnView::default();
        push.turn_id = "t-4".into();
        push.source = TurnSource::Push;
        push.reply = "Recuerda la cita.".into();
        push.outcome = Some(TurnOutcome::Error);
        push.error_code = Some("TURN_ERROR".into());
        push.audio = TurnAudio::Muted;
        let mut c = ConversationView::default();
        c.activity = UserActivity::Recording;
        c.live = "pon la".into();
        c.draft = Some("borrador".into());
        c.turns = vec![t, push];
        c.speaking = true;
        c.awaiting_visible = Some("t-4".into());
        c.mic = MicPermission::Granted;
        c.transcriber = ServiceState::Degraded;
        c.tts = ServiceState::Up;
        c.notice = Some(Notice::SilenceCut);
        c
    }

    fn v4_requests() -> Vec<IpcRequest> {
        vec![
            IpcRequest::SubscribeConversation,
            IpcRequest::SendText {
                text: UserText("hola \"Fogar\"\n".into()),
            },
            IpcRequest::CaptureStart {
                gesture: CaptureGesture::Hold,
            },
            IpcRequest::CaptureStart {
                gesture: CaptureGesture::Toggle,
            },
            IpcRequest::CaptureEnd {
                then: CaptureThen::Send,
            },
            IpcRequest::CaptureEnd {
                then: CaptureThen::Review,
            },
            IpcRequest::CaptureEnd {
                then: CaptureThen::Discard,
            },
            IpcRequest::CancelTurn,
            IpcRequest::TurnVisible {
                turn_id: "t-4".into(),
            },
            IpcRequest::RequestMicPermission,
        ]
    }

    #[test]
    fn framing_limits() {
        let mut b = vec![0, 0, 0, 2, b'{', b'}', 9];
        assert_eq!(
            take_frame(&mut b).unwrap().as_deref(),
            Some(&b"{}".to_vec())
        );
        assert_eq!(b, vec![9]);
        let mut big = (u32::try_from(MAX_FRAME_BYTES).unwrap() + 1)
            .to_be_bytes()
            .to_vec();
        assert!(matches!(
            take_frame(&mut big),
            Err(IpcError::TooLarge { .. })
        ));
        assert!(matches!(
            frame(&vec![0; MAX_FRAME_BYTES + 1]),
            Err(IpcError::TooLarge { .. })
        ));
        let f = frame(b"{}").unwrap();
        let mut half = f[..3].to_vec();
        assert_eq!(take_frame(&mut half).unwrap(), None);
        assert_eq!(half.len(), 3, "partial frames stay buffered");
    }

    #[test]
    fn requests_round_trip_and_the_vocabulary_is_closed() {
        for r in [
            IpcRequest::Hello { version: 2 },
            IpcRequest::Status,
            IpcRequest::Subscribe,
            IpcRequest::Discover,
            IpcRequest::Unpair,
            IpcRequest::Reconnect,
            IpcRequest::Pair {
                address: "green-house".into(),
                code: PairCode::parse("482 913"),
            },
        ]
        .into_iter()
        .chain(v4_requests())
        {
            assert_eq!(decode_request(&encode_request(&r).unwrap()).unwrap(), r);
        }
        assert_eq!(
            decode_request(br#"{"type":"status"}"#).unwrap(),
            IpcRequest::Status
        );
        // v4 vocabulary is closed too.
        for bad in [
            &br#"{"type":"send_text","data":{"text":"x","agent":"root"}}"#[..],
            br#"{"type":"send_text"}"#,
            br#"{"type":"capture_start","data":{"gesture":"wake_word"}}"#,
            br#"{"type":"capture_end","data":{"then":"upload"}}"#,
            br#"{"type":"capture_start"}"#,
            br#"{"type":"turn_visible","data":{}}"#,
            br#"{"type":"cancel_turn","data":{"turn_id":"t-1"}}"#,
            br#"{"type":"open_microphone"}"#,
        ] {
            assert!(
                decode_request(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        assert!(decode_request(br#"{"type":"status","exec":"rm -rf"}"#).is_err());
        assert!(decode_request(br#"{"type":"run","data":{"cmd":"x"}}"#).is_err());
        assert!(decode_request(br#"{"type":"pair","data":{"address":"x"}}"#).is_err());
        assert!(decode_request(br#"{"type":"hello","data":{"version":2,"admin":true}}"#).is_err());
        // A typo is decoded (so the core can answer BadCode), not dropped as malformed.
        assert_eq!(
            decode_request(br#"{"type":"pair","data":{"address":"x","code":"12a456"}}"#).unwrap(),
            IpcRequest::Pair {
                address: "x".into(),
                code: None
            }
        );
    }

    #[test]
    fn responses_round_trip() {
        for r in [
            IpcResponse::Hello { version: 2 },
            IpcResponse::Status(report()),
            IpcResponse::Subscribed,
            IpcResponse::Event(report()),
            IpcResponse::Discovered(vec![GatewayCandidate::new(
                "http://127.0.0.1:8004".into(),
                "green-house".into(),
                "1.29.0".into(),
            )]),
            IpcResponse::Paired(report().gateway.unwrap()),
            IpcResponse::PairFailed(PairFailure::InvalidCode),
            IpcResponse::Unpaired,
            IpcResponse::Reconnecting,
            IpcResponse::ConversationSubscribed,
            IpcResponse::Conversation(conversation()),
            IpcResponse::Conversation(ConversationView::default()),
            IpcResponse::CaptureStarted,
            IpcResponse::CaptureEnded,
            IpcResponse::TextAccepted,
            IpcResponse::TurnCancelSent,
            IpcResponse::TurnVisibleNoted,
        ]
        .into_iter()
        .chain(
            [
                CaptureRefusal::NotSubscribed,
                CaptureRefusal::PermissionNotDetermined,
                CaptureRefusal::PermissionDenied,
                CaptureRefusal::NoMicrophone,
                CaptureRefusal::NotReady,
                CaptureRefusal::NoTranscriber,
                CaptureRefusal::Busy,
            ]
            .map(IpcResponse::CaptureRefused),
        )
        .chain(
            [
                TextRefusal::Empty,
                TextRefusal::TooLong,
                TextRefusal::NotReady,
                TextRefusal::Recording,
            ]
            .map(IpcResponse::TextRefused),
        )
        .chain(
            [
                MicPermission::Granted,
                MicPermission::Denied,
                MicPermission::NotDetermined,
                MicPermission::Restricted,
                MicPermission::Unknown,
            ]
            .map(IpcResponse::MicPermission),
        ) {
            assert_eq!(
                decode_response(&encode_response(r.clone()).unwrap()).unwrap(),
                r
            );
        }
    }

    /// Whatever the content, a pushed conversation always fits one frame (an encode error would
    /// make the watch resend the same oversized view on every change and freeze the app).
    #[test]
    fn conversation_always_fits_a_frame() {
        let quotes = "\"".repeat(60 * 1024); // 2x when escaped
        let controls = "\u{1}".repeat(60 * 1024); // 6x when escaped
        let wide = "ñ".repeat(30 * 1024);
        for text in [quotes, controls, wide] {
            // One huge reply (dropping old turns cannot help) ...
            let mut c = conversation();
            c.turns[1].reply = text.clone();
            // ... and many big turns, plus big live/draft.
            let mut many = conversation();
            many.live = text.clone();
            many.draft = Some(text.clone());
            many.turns = (0..20)
                .map(|i| {
                    let mut t = TurnView::default();
                    t.turn_id = format!("t-{i}");
                    t.reply = text.clone();
                    t
                })
                .collect();
            for view in [c, many] {
                let body = encode_response(IpcResponse::Conversation(view)).unwrap();
                assert!(body.len() <= MAX_FRAME_BYTES, "{} B", body.len());
                assert!(frame(&body).is_ok());
                let IpcResponse::Conversation(back) = decode_response(&body).unwrap() else {
                    panic!("not a conversation")
                };
                assert!(!back.turns.is_empty(), "the newest turn survives");
            }
        }
        // Gateway-controlled ids (a 70 KB turn_id, conversation or error code) never block a
        // push either.
        let huge_id = "\u{1}".repeat(70 * 1024);
        let mut ids = conversation();
        ids.awaiting_visible = Some(huge_id.clone());
        ids.turns[0].turn_id = huge_id.clone();
        ids.turns[0].conversation = Some(huge_id.clone());
        ids.turns[1].error_code = Some(huge_id);
        let body = encode_response(IpcResponse::Conversation(ids)).unwrap();
        assert!(body.len() <= MAX_FRAME_BYTES);
        let IpcResponse::Conversation(back) = decode_response(&body).unwrap() else {
            panic!()
        };
        assert_eq!(
            back.awaiting_visible, None,
            "an id the UI could not echo is dropped"
        );
        // A view that fits is sent untouched.
        let small = conversation();
        let IpcResponse::Conversation(back) =
            decode_response(&encode_response(IpcResponse::Conversation(small.clone())).unwrap())
                .unwrap()
        else {
            panic!()
        };
        assert_eq!(back, small);
    }

    #[test]
    fn user_content_never_prints() {
        let secret = "mi cita con el médico";
        let r = IpcRequest::SendText {
            text: UserText(secret.into()),
        };
        assert!(!format!("{r:?}").contains(secret));
        let mut c = conversation();
        c.live = secret.into();
        c.draft = Some(secret.into());
        c.turns[0].mine = secret.into();
        c.turns[0].reply = secret.into();
        let resp = IpcResponse::Conversation(c.clone());
        assert!(!format!("{resp:?}").contains(secret));
        assert!(!format!("{:?}", conversation_to_wire(c)).contains(secret));
        let w: WireRequest = serde_json::from_slice(&encode_request(&r).unwrap()).unwrap();
        assert!(!format!("{w:?}").contains(secret));
    }

    /// Golden JSON: this is what the Swift app parses. Changing it is an IPC_VERSION bump.
    #[test]
    fn golden_json() {
        let s = String::from_utf8(encode_response(IpcResponse::Event(report())).unwrap()).unwrap();
        assert_eq!(
            s,
            r#"{"type":"event","data":{"link":"ready","phase":"ready","gateway":{"address":"http://green-house:8004","name":"green-house","version":"1.29.0","client_name":"Mac de Sito"},"sessions":3,"turns_completed":7,"boot_mode":"normal","peer_check":"same_signer","transcriber":"up","tts":"down"}}"#
        );
        let s =
            String::from_utf8(encode_response(IpcResponse::Conversation(conversation())).unwrap())
                .unwrap();
        assert_eq!(
            s,
            r#"{"type":"conversation","data":{"activity":"recording","live":"pon la","draft":"borrador","turns":[{"turn_id":"t-3","conversation":"0b9d","source":"mine","mine":"¿qué tiempo hace?","reply":"Soleado.","progress":"tool","tool":"ha_call_service","outcome":null,"error_code":null,"audio":"none"},{"turn_id":"t-4","conversation":null,"source":"push","mine":"","reply":"Recuerda la cita.","progress":null,"tool":null,"outcome":"error","error_code":"TURN_ERROR","audio":"muted"}],"speaking":true,"awaiting_visible":"t-4","mic":"granted","transcriber":"degraded","tts":"up","notice":"silence_cut"}}"#
        );
        for (resp, json) in [
            (
                IpcResponse::ConversationSubscribed,
                r#"{"type":"conversation_subscribed"}"#,
            ),
            (IpcResponse::CaptureStarted, r#"{"type":"capture_started"}"#),
            (
                IpcResponse::CaptureRefused(CaptureRefusal::PermissionNotDetermined),
                r#"{"type":"capture_refused","data":{"reason":"permission_not_determined"}}"#,
            ),
            (IpcResponse::CaptureEnded, r#"{"type":"capture_ended"}"#),
            (IpcResponse::TextAccepted, r#"{"type":"text_accepted"}"#),
            (
                IpcResponse::TextRefused(TextRefusal::TooLong),
                r#"{"type":"text_refused","data":{"reason":"too_long"}}"#,
            ),
            (
                IpcResponse::TurnCancelSent,
                r#"{"type":"turn_cancel_sent"}"#,
            ),
            (
                IpcResponse::TurnVisibleNoted,
                r#"{"type":"turn_visible_noted"}"#,
            ),
            (
                IpcResponse::MicPermission(MicPermission::NotDetermined),
                r#"{"type":"mic_permission","data":{"state":"not_determined"}}"#,
            ),
        ] {
            assert_eq!(
                String::from_utf8(encode_response(resp).unwrap()).unwrap(),
                json
            );
        }
        for (req, json) in v4_requests().iter().zip([
            r#"{"type":"subscribe_conversation"}"#,
            r#"{"type":"send_text","data":{"text":"hola \"Fogar\"\n"}}"#,
            r#"{"type":"capture_start","data":{"gesture":"hold"}}"#,
            r#"{"type":"capture_start","data":{"gesture":"toggle"}}"#,
            r#"{"type":"capture_end","data":{"then":"send"}}"#,
            r#"{"type":"capture_end","data":{"then":"review"}}"#,
            r#"{"type":"capture_end","data":{"then":"discard"}}"#,
            r#"{"type":"cancel_turn"}"#,
            r#"{"type":"turn_visible","data":{"turn_id":"t-4"}}"#,
            r#"{"type":"request_mic_permission"}"#,
        ]) {
            assert_eq!(
                std::str::from_utf8(&encode_request(req).unwrap()).unwrap(),
                json
            );
        }
        let s = String::from_utf8(
            encode_response(IpcResponse::PairFailed(PairFailure::InvalidCode)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            s,
            r#"{"type":"pair_failed","data":{"reason":"invalid_code"}}"#
        );
        let s = String::from_utf8(encode_response(IpcResponse::Subscribed).unwrap()).unwrap();
        assert_eq!(s, r#"{"type":"subscribed"}"#);
        let s = String::from_utf8(encode_response(IpcResponse::Reconnecting).unwrap()).unwrap();
        assert_eq!(s, r#"{"type":"reconnecting"}"#);
        let r = encode_request(&IpcRequest::Reconnect).unwrap();
        assert_eq!(std::str::from_utf8(&r).unwrap(), r#"{"type":"reconnect"}"#);
        let r = encode_request(&IpcRequest::Pair {
            address: "green-house".into(),
            code: PairCode::parse("482913"),
        })
        .unwrap();
        assert_eq!(
            std::str::from_utf8(&r).unwrap(),
            r#"{"type":"pair","data":{"address":"green-house","code":"482913"}}"#
        );
    }

    #[test]
    fn pair_code_never_prints() {
        let r = IpcRequest::Pair {
            address: "x".into(),
            code: PairCode::parse("482913"),
        };
        assert!(!format!("{r:?}").contains("482913"));
    }
}
