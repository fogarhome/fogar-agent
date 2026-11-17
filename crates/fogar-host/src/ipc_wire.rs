//! The local UI wire format (ADR-0002, ADR-0004), shared by every OS's IPC adapter and the single
//! source for the Swift types (typeshare, S1): 4-byte big-endian length + one JSON object, at most
//! `MAX_FRAME_BYTES`, `"type"`-tagged, `deny_unknown_fields`, closed vocabulary.
//!
//! Sans-I/O: framing works on byte buffers; adapters own the socket and peer authentication.
//! Pairing codes cross this layer once; the frame buffers holding them are wiped. serde_json's
//! internal scratch for escaped strings is not (codes are 6 digits, never escaped). No key
//! ever crosses IPC.

use fogar_ports::ipc::{
    GatewayCandidate, GatewayInfo, IpcError, IpcRequest, IpcResponse, LinkPhase, MAX_FRAME_BYTES,
    PairCode, PairFailure, PeerCheck, StatusReport,
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
        ] {
            assert_eq!(decode_request(&encode_request(&r).unwrap()).unwrap(), r);
        }
        assert_eq!(
            decode_request(br#"{"type":"status"}"#).unwrap(),
            IpcRequest::Status
        );
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
        ] {
            assert_eq!(
                decode_response(&encode_response(r.clone()).unwrap()).unwrap(),
                r
            );
        }
    }

    /// Golden JSON: this is what the Swift app parses. Changing it is an IPC_VERSION bump.
    #[test]
    fn golden_json() {
        let s = String::from_utf8(encode_response(IpcResponse::Event(report())).unwrap()).unwrap();
        assert_eq!(
            s,
            r#"{"type":"event","data":{"link":"ready","phase":"ready","gateway":{"address":"http://green-house:8004","name":"green-house","version":"1.29.0","client_name":"Mac de Sito"},"sessions":3,"turns_completed":7,"boot_mode":"normal","peer_check":"same_signer"}}"#
        );
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
