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

pub trait GatewayLink: Send + Sync + 'static {
    type Session: LinkSession;
    /// Opens a new transport session to `addr` (TCP [+TLS] + WS upgrade on `/ws/stream` for the
    /// real adapter). The address comes from the pairing record, so it can change between calls.
    /// The adapter refuses addresses its transport policy forbids (e.g. clear text to a public
    /// host) with `NotConfigured`. The core wraps this in its own deadline; dropping the future
    /// must not leak a half-open socket.
    fn connect(
        &self,
        addr: &GatewayAddr,
    ) -> impl Future<Output = Result<Self::Session, LinkError>> + Send;
}

/// Default gateway port (client-protocol §0).
pub const DEFAULT_GATEWAY_PORT: u16 = 8004;

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
    /// or `wss://` in front (an optional trailing `/` or `/ws/stream` is ignored). Without a
    /// scheme the address is plain text: GUESSED from client-protocol §0, which gives bare
    /// `host:port` for LAN/tailnet and says nothing about how a client picks TLS.
    pub fn parse(input: &str) -> Result<Self, AddrError> {
        let s = input.trim();
        let (scheme, rest) = match s.split_once("://") {
            Some(("http" | "ws", r)) => (Scheme::Plain, r),
            Some(("https" | "wss", r)) => (Scheme::Tls, r),
            Some(_) => return Err(AddrError),
            None => (Scheme::Plain, s),
        };
        let rest = rest
            .strip_suffix("/ws/stream")
            .or_else(|| rest.strip_suffix('/'))
            .unwrap_or(rest);
        if rest.is_empty()
            || rest.len() > 255
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
        if host.is_empty()
            || !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':' | '_'))
        {
            return Err(AddrError);
        }
        let port = match port {
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
    fn parses_the_forms_people_type() {
        let a = GatewayAddr::parse("green-house").unwrap();
        assert_eq!(
            (a.scheme(), a.host(), a.port()),
            (Scheme::Plain, "green-house", 8004)
        );
        let a = GatewayAddr::parse(" 192.168.1.19:9000 ").unwrap();
        assert_eq!((a.host(), a.port()), ("192.168.1.19", 9000));
        let a = GatewayAddr::parse("https://Fogar.Example.com/").unwrap();
        assert_eq!(
            (a.scheme(), a.host(), a.port()),
            (Scheme::Tls, "fogar.example.com", 8004)
        );
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
        ] {
            assert_eq!(GatewayAddr::parse(bad), Err(AddrError), "{bad:?}");
        }
        assert!(GatewayAddr::parse(&"a".repeat(300)).is_err());
    }
}
