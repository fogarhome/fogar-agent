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
    Closed {
        code: u16,
        reason: String,
    },
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
    fn close(
        &mut self,
        code: u16,
        reason: &str,
    ) -> impl Future<Output = Result<(), LinkError>> + Send;
}

/// Which WebSocket endpoint a session opens. The conversation core always uses `Stream`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WsRoute {
    /// `/ws/stream`: the conversation socket (spec §1).
    Stream,
    /// `/speech/dictation`: GUESSED (JGW Spec 5.0, Voz sin el WebSocket de conversación)
    Dictation,
}

impl WsRoute {
    pub fn path(self) -> &'static str {
        match self {
            Self::Stream => "/ws/stream",
            Self::Dictation => "/speech/dictation",
        }
    }
}

pub trait GatewayLink: Send + Sync + 'static {
    type Session: LinkSession;
    /// Opens a new transport session to `addr` (TCP [+TLS] + WS upgrade on `route.path()` for the
    /// real adapter). The address comes from the pairing record, so it can change between calls.
    /// The session carries the account token, so for `Scheme::Plain` the adapter applies
    /// [`secret_cleartext_allowed`] (loopback or tailnet only) to EVERY resolved socket
    /// address before sending a byte (not to the host string: a LAN name can resolve to a public
    /// IP on a hostile network) and refuses with `NotConfigured`; it also refuses `Scheme::Tls`
    /// when it has no TLS. The core wraps this in its own deadline; dropping the future
    /// must not leak a half-open socket.
    fn connect(
        &self,
        addr: &GatewayAddr,
        route: WsRoute,
    ) -> impl Future<Output = Result<Self::Session, LinkError>> + Send;
}

/// Default gateway port (client-protocol §0).
pub const DEFAULT_GATEWAY_PORT: u16 = 8004;
/// Default port for `https://`/`wss://` addresses without one: a TLS proxy in front of the
/// gateway. GUESSED (the protocol only names 8004 for plain LAN/tailnet access).
pub const DEFAULT_TLS_PORT: u16 = 443;

/// Clear-text policy for calls WITHOUT a secret (discovery, `/.well-known`, NET-1): unencrypted
/// only to loopback, private LAN (RFC 1918), Tailscale/CGNAT (100.64.0.0/10), IPv6 ULA
/// (fc00::/7) and link-local addresses. IPv4-mapped IPv6 is judged as IPv4. Everything else
/// needs TLS. Calls that carry a secret use the stricter [`secret_cleartext_allowed`].
pub fn cleartext_allowed(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || (o[0] == 100 && (o[1] & 0xC0) == 64)
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => cleartext_allowed(IpAddr::V4(v4)),
            None => {
                let seg0 = v6.segments()[0];
                v6.is_loopback() || (seg0 & 0xfe00) == 0xfc00 || (seg0 & 0xffc0) == 0xfe80
            }
        },
    }
}

/// Clear-text policy for a call that carries a secret (the account password or token: every
/// HTTP call except `/.well-known`, every WebSocket connect). Option B (NV-1): only loopback and
/// the Tailscale tailnet ([`crate::tailnet::is_tailnet_ip`]), which WireGuard already encrypts.
/// The home LAN (RFC 1918), link-local and ULA are refused: anyone on the Wi-Fi could read the
/// password. IPv4-mapped IPv6 is judged as IPv4.
pub fn secret_cleartext_allowed(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    ip.is_loopback() || crate::tailnet::is_tailnet_ip(ip)
}

/// Transport security of a gateway address. LAN and tailnet gateways speak clear text
/// (`http`/`ws`); behind a TLS proxy, `https`/`wss` (client-protocol §0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scheme {
    Plain,
    Tls,
}

/// Where a gateway lives: `host[:port]` plus transport security. Validated user input (the
/// person may type it): no credentials, no path, no whitespace, port 1..=65535.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GatewayAddr {
    scheme: Scheme,
    host: String,
    port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not a gateway address (expected host[:port], optionally with http:// or https://)")]
pub struct AddrError;

impl GatewayAddr {
    /// Accepts `host`, `host:port`, `[v6]:port` and the same with `http://`, `https://`, `ws://`
    /// or `wss://` in front (any case; an optional trailing `/`, `/ws/stream` or `/ws/stream/`
    /// is ignored). Without a scheme the address is plain text: GUESSED from client-protocol §0,
    /// which gives bare `host:port` for LAN/tailnet and says nothing about how a client picks
    /// TLS; [`cleartext_allowed`] keeps such an address off the public internet. IPv6 zone ids
    /// (`%en0`) are rejected on purpose (link-local gateways are not a supported setup).
    pub fn parse(input: &str) -> Result<Self, AddrError> {
        let s = input.trim();
        let (scheme, rest) = match s.split_once("://") {
            Some((sch, r)) => match sch.to_ascii_lowercase().as_str() {
                "http" | "ws" => (Scheme::Plain, r),
                "https" | "wss" => (Scheme::Tls, r),
                _ => return Err(AddrError),
            },
            None => (Scheme::Plain, s),
        };
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        let rest = rest.strip_suffix("/ws/stream").unwrap_or(rest);
        if rest.is_empty()
            || rest
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '@' | '?' | '#'))
        {
            return Err(AddrError);
        }
        let (host, port) = if let Some(v6) = rest.strip_prefix('[') {
            let (h, after) = v6.split_once(']').ok_or(AddrError)?;
            let port = match after {
                "" => None,
                p => Some(p.strip_prefix(':').ok_or(AddrError)?),
            };
            if h.is_empty()
                || !h
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
            {
                return Err(AddrError);
            }
            (h, port)
        } else {
            match rest.split_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (rest, None),
            }
        };
        // The bound is on the host, not on the whole input: `Display` always adds `:port`, and
        // what it prints must parse again (a stored pairing is read back that way).
        if host.is_empty()
            || host.len() > 253
            || !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':' | '_'))
        {
            return Err(AddrError);
        }
        let port = match port {
            None if scheme == Scheme::Tls => DEFAULT_TLS_PORT,
            None => DEFAULT_GATEWAY_PORT,
            Some(p) => match p.parse::<u16>() {
                Ok(0) | Err(_) => return Err(AddrError),
                Ok(n) => n,
            },
        };
        Ok(Self {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
        })
    }

    pub fn scheme(&self) -> Scheme {
        self.scheme
    }
    /// Host without brackets (IPv6 literals are bare).
    pub fn host(&self) -> &str {
        &self.host
    }
    pub fn port(&self) -> u16 {
        self.port
    }
    /// `host:port` with IPv6 literals bracketed (for URLs and `Host:` headers).
    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// `http(s)://host:port` form, round-trips through [`GatewayAddr::parse`].
impl fmt::Display for GatewayAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self.scheme {
            Scheme::Plain => "http",
            Scheme::Tls => "https",
        };
        write!(f, "{s}://{}", self.authority())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_cleartext_only_loopback_and_tailnet() {
        let ip = |s: &str| {
            s.parse::<std::net::IpAddr>()
                .unwrap_or_else(|_| panic!("{s}"))
        };
        for lan in [
            "192.168.1.19",
            "10.0.0.1",
            "172.16.0.1",
            "169.254.1.1",
            "fd00::1",
        ] {
            assert!(!secret_cleartext_allowed(ip(lan)), "{lan}");
            assert!(cleartext_allowed(ip(lan)), "discovery keeps {lan}");
        }
        for other in [
            "8.8.8.8",
            "::ffff:192.168.1.19",
            "2001:db8::1",
            "100.128.0.1",
        ] {
            assert!(!secret_cleartext_allowed(ip(other)), "{other}");
        }
        for ok in [
            "127.0.0.1",
            "::1",
            "100.89.15.7",
            "fd7a:115c:a1e0::1",
            "::ffff:100.89.15.7",
            "::ffff:127.0.0.1",
        ] {
            assert!(secret_cleartext_allowed(ip(ok)), "{ok}");
        }
    }

    /// Fuzz find (fuzz/ ipc_frames): a 251-255 char host without a port parsed, but its
    /// `Display` form (which always adds `:port`) did not, so a stored pairing could not be read back.
    #[test]
    fn every_accepted_address_round_trips_through_display() {
        for n in 245..=260 {
            for input in [
                "a".repeat(n),
                format!("https://{}", "a".repeat(n)),
                format!("[{}]", "1".repeat(n)),
                format!("{}:65535", "a".repeat(n)),
            ] {
                if let Ok(a) = GatewayAddr::parse(&input) {
                    assert_eq!(
                        GatewayAddr::parse(&a.to_string()).ok(),
                        Some(a),
                        "{n}: {input}"
                    );
                }
            }
        }
        assert!(GatewayAddr::parse(&"a".repeat(253)).is_ok());
    }

    #[test]
    fn parses_the_forms_people_type() {
        let a = GatewayAddr::parse("green-house").unwrap();
        assert_eq!(
            (a.scheme(), a.host(), a.port()),
            (Scheme::Plain, "green-house", 8004)
        );
        let a = GatewayAddr::parse(" 192.168.1.19:9000 ").unwrap();
        assert_eq!((a.host(), a.port()), ("192.168.1.19", 9000));
        let a = GatewayAddr::parse("HTTPS://Fogar.Example.com/").unwrap();
        assert_eq!(
            (a.scheme(), a.host(), a.port()),
            (Scheme::Tls, "fogar.example.com", 443)
        );
        let a = GatewayAddr::parse("wss://gw.example:8443/ws/stream/").unwrap();
        assert_eq!((a.scheme(), a.port()), (Scheme::Tls, 8443));
        let a = GatewayAddr::parse("ws://127.0.0.1:8004/ws/stream").unwrap();
        assert_eq!(a.authority(), "127.0.0.1:8004");
        let a = GatewayAddr::parse("[fd7a:115c::1]:8004").unwrap();
        assert_eq!(
            (a.host(), a.authority().as_str()),
            ("fd7a:115c::1", "[fd7a:115c::1]:8004")
        );
        let a = GatewayAddr::parse("gw.tail1234.ts.net").unwrap();
        assert_eq!(GatewayAddr::parse(&a.to_string()), Ok(a));
    }

    #[test]
    fn cleartext_only_to_private_networks() {
        let ok = |s: &str| cleartext_allowed(s.parse().unwrap());
        for a in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.19",
            "100.64.0.1",
            "100.127.255.254",
            "169.254.1.1",
            "::1",
            "fd7a:115c:a1e0::1",
            "fe80::1",
            "::ffff:192.168.1.19",
        ] {
            assert!(ok(a), "{a}");
        }
        for a in [
            "8.8.8.8",
            "203.0.113.7",
            "100.128.0.1",
            "100.63.255.255",
            "172.32.0.1",
            "2001:4860::8888",
            "::ffff:8.8.8.8",
            "0.0.0.0",
            "::",
        ] {
            assert!(!ok(a), "{a}");
        }
    }

    #[test]
    fn rejects_everything_else() {
        for bad in [
            "",
            " ",
            "ftp://x",
            "u:p@host",
            "host/path",
            "host:0",
            "host:70000",
            "host:",
            "ho st",
            "host?x",
            "[::1",
            "[]:80",
            "http://",
            "host#f",
            "h\u{0}x",
            "[fe80::1%en0]",
        ] {
            assert_eq!(GatewayAddr::parse(bad), Err(AddrError), "{bad:?}");
        }
        assert!(GatewayAddr::parse(&"a".repeat(300)).is_err());
    }
}
