//! GatewayHttp port: the gateway's closed REST call set (client-protocol §0 and JGW Spec 5.0):
//! discovery (`/.well-known/*`), pairing (`POST /pair`), login/session and the conversation
//! calls. [`HttpCall`] is the whole set: fixed method and path per variant, so the port cannot
//! be used as a generic HTTP client.
//!
//! TRANSPORT ONLY, like `GatewayLink`: status code + body bytes. Request bodies and response
//! parsing live in fogar-proto + fogar-core, so the adapter cannot drift from the protocol and
//! the fake exercises the real codec.
//!
//! - ASYNC (real network latency), desugared `fn -> impl Future + Send`. Not dyn-compatible: the
//!   core is generic over `H: GatewayHttp`.
//! - Bearer token: authenticated calls (`HttpCall::authenticated`) carry
//!   `Authorization: Bearer <token>`. A missing, empty or non-printable-ASCII token is `Refused`
//!   before a byte is sent, and so is a token on an unauthenticated call (discovery, pairing,
//!   login): it would leak to a gateway that has not been authenticated. The adapter writes the token straight from its secret buffer into
//!   the socket; it is never copied into a `String`, a log or an error.
//! - Streaming calls (`HttpCall::is_stream`) are refused by `request`: they go through
//!   `open_stream`, which returns the status once the response head arrived and a
//!   [`ResponseBody`] read chunk by chunk (at most `MAX_CHUNK_BYTES` each). Dial, request and
//!   response head run under the adapter's call deadline; the body has none (the core wraps
//!   every `chunk()`), no total cap (on a non-2xx status the core caps it at `MAX_BODY_BYTES`),
//!   and dropping it closes the connection. `chunk()` is cancel-safe: the adapter keeps read
//!   bytes and decoder state in the body, never in the future.
//! - NOT select branches: the core wraps each call in its own deadline (1–2 s per probe, spec §0)
//!   and drops it on expiry. Dropping an in-flight call must not leak a socket or poison the
//!   adapter: the next call works.
//! - Transport policy: `Scheme::Plain` only when `gateway::cleartext_allowed` holds for every
//!   resolved socket address (checked before sending a byte; else `Refused`). `Scheme::Tls`
//!   verifies the certificate against the platform roots; no "accept invalid" switch.
//! - One request per connection (`Connection: close`), no pooling: a cancelled call can never
//!   hand a half-read connection to the next one.
//! - Per-call body cap (`HttpCall::max_body`: `MAX_BODY_BYTES`, or `MAX_LIST_BODY_BYTES` for
//!   list replies) enforced WHILE reading: a larger or endless body (lying `Content-Length`,
//!   `chunked`) is `TooLarge` as soon as the limit is crossed, never buffered first. `chunked`
//!   replies are decoded; the cap counts decoded bytes. It never follows redirects (a redirect
//!   could move the key exchange to another host).
//! - Bodies are wiped from memory on drop and never printed: the `/pair` reply carries the
//!   `client_key` exactly once. Adapters read the body into ONE buffer pre-sized from
//!   `Content-Length` (capped at the call's limit), so growth never leaves stray copies.
//!
//! How Windows/Linux would do this: the same portable HTTP/1.1 client (fogar-host); nothing OS
//! specific, so no capabilities.

use crate::gateway::GatewayAddr;
use secrecy::{ExposeSecret, SecretString, zeroize::Zeroizing};
use std::{fmt, future::Future};

/// Default response body cap (`HttpCall::max_body`): replies are small JSON objects.
pub const MAX_BODY_BYTES: usize = 16 * 1024;

/// Largest piece `ResponseBody::chunk` returns.
pub const MAX_CHUNK_BYTES: usize = 16 * 1024;

/// Longest `Last-Event-ID` accepted by `open_stream`.
pub const MAX_LAST_EVENT_ID: usize = 128;

/// Largest response body for list-shaped replies (conversations, messages, agents).
pub const MAX_LIST_BODY_BYTES: usize = 256 * 1024;

/// An id that is safe to put in a URL path as-is: `[A-Za-z0-9_.:-]{1,128}`. Not a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathId(String);

impl PathId {
    pub fn new(s: &str) -> Option<PathId> {
        let ok = (1..=128).contains(&s.len())
            && !matches!(s, "." | "..") // dot-segments would be normalised out of the closed route set
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'));
        ok.then(|| PathId(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The calls the core may make. Closed set with fixed routes: the port cannot be used as a
/// generic HTTP client. The well-known paths equal `fogar_proto::pairing::WELL_KNOWN_PATHS`
/// (checked by a fogar-core test; ports do not depend on proto).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HttpCall {
    /// `GET /.well-known/fogar-gateway`.
    WellKnown,
    /// `GET /.well-known/jota-gateway` (gateways from before the Fogar rename).
    WellKnownLegacy,
    /// `POST /pair` with a JSON body.
    Pair,
    /// `POST /auth/login`. GUESSED (JGW Spec 5.0, Mapa de la API)
    Login,
    /// `POST /auth/logout`. GUESSED (JGW Spec 5.0, Mapa de la API)
    Logout,
    /// `GET /auth/me`. GUESSED (JGW Spec 5.0, Mapa de la API)
    Me,
    /// `PATCH /auth/me`. GUESSED (JGW Spec 5.0, Mapa de la API)
    UpdateMe,
    /// `GET /conversations`. GUESSED (JGW Spec 5.0, Mapa de la API)
    ListConversations,
    /// `GET /conversations/agents`. GUESSED (JGW Spec 5.0, Mapa de la API)
    ListAgents,
    /// `GET /conversations/{id}`. GUESSED (JGW Spec 5.0, Mapa de la API)
    GetConversation { id: PathId },
    /// `GET /conversations/{id}/messages[?before=]`. GUESSED (JGW Spec 5.0, Mapa de la API)
    GetMessages { id: PathId, before: Option<PathId> },
    /// `POST /conversations`. GUESSED (JGW Spec 5.0, Mapa de la API)
    CreateConversation,
    /// `POST /conversations/{id}/messages`. GUESSED (JGW Spec 5.0, Mapa de la API)
    PostMessage { id: PathId },
    /// `DELETE /conversations/{id}/messages/{message}`. GUESSED (JGW Spec 5.0, Mapa de la API)
    Withdraw { id: PathId, message: PathId },
    /// `POST /conversations/{id}/cancel`. GUESSED (JGW Spec 5.0, Mapa de la API)
    Cancel { id: PathId },
    /// `GET /events[?audio=1]` (stream). GUESSED (JGW Spec 5.0, Mapa de la API)
    Events { audio: bool },
    /// `POST /conversations/{id}/messages/{message}/speech` (stream).
    /// GUESSED (JGW Spec 5.0, Mapa de la API)
    Speech { id: PathId, message: PathId },
}

impl HttpCall {
    pub fn method(&self) -> &'static str {
        use HttpCall::*;
        match self {
            WellKnown
            | WellKnownLegacy
            | Me
            | ListConversations
            | ListAgents
            | GetConversation { .. }
            | GetMessages { .. }
            | Events { .. } => "GET",
            UpdateMe => "PATCH",
            Withdraw { .. } => "DELETE",
            Pair
            | Login
            | Logout
            | CreateConversation
            | PostMessage { .. }
            | Cancel { .. }
            | Speech { .. } => "POST",
        }
    }

    pub fn path(&self) -> String {
        use HttpCall::*;
        match self {
            WellKnown => "/.well-known/fogar-gateway".into(),
            WellKnownLegacy => "/.well-known/jota-gateway".into(),
            Pair => "/pair".into(),
            Login => "/auth/login".into(),
            Logout => "/auth/logout".into(),
            Me | UpdateMe => "/auth/me".into(),
            ListConversations | CreateConversation => "/conversations".into(),
            ListAgents => "/conversations/agents".into(),
            GetConversation { id } => format!("/conversations/{}", id.as_str()),
            GetMessages { id, before: None } => format!("/conversations/{}/messages", id.as_str()),
            GetMessages {
                id,
                before: Some(b),
            } => format!(
                "/conversations/{}/messages?before={}",
                id.as_str(),
                b.as_str()
            ),
            PostMessage { id } => format!("/conversations/{}/messages", id.as_str()),
            Withdraw { id, message } => {
                format!(
                    "/conversations/{}/messages/{}",
                    id.as_str(),
                    message.as_str()
                )
            }
            Cancel { id } => format!("/conversations/{}/cancel", id.as_str()),
            Events { audio: false } => "/events".into(),
            Events { audio: true } => "/events?audio=1".into(),
            Speech { id, message } => format!(
                "/conversations/{}/messages/{}/speech",
                id.as_str(),
                message.as_str()
            ),
        }
    }

    pub fn max_body(&self) -> usize {
        use HttpCall::*;
        match self {
            ListConversations | ListAgents | GetMessages { .. } => MAX_LIST_BODY_BYTES,
            _ => MAX_BODY_BYTES,
        }
    }

    pub fn is_stream(&self) -> bool {
        matches!(self, HttpCall::Events { .. } | HttpCall::Speech { .. })
    }

    /// Needs the session credential (everything except discovery, pairing and login).
    pub fn authenticated(&self) -> bool {
        !matches!(
            self,
            HttpCall::WellKnown | HttpCall::WellKnownLegacy | HttpCall::Pair | HttpCall::Login
        )
    }
}

/// A complete HTTP response. The body is zeroized on drop.
pub struct HttpReply {
    pub status: u16,
    pub body: Zeroizing<Vec<u8>>,
}

impl HttpReply {
    pub fn new(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            body: Zeroizing::new(body),
        }
    }
}

/// Never print the body (it may carry the client key): only status + size.
impl fmt::Debug for HttpReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HttpReply({}, {} bytes)", self.status, self.body.len())
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HttpError {
    /// Nothing answered (refused, DNS failure, no route). The core tries the next candidate.
    #[error("gateway unreachable")]
    Unreachable,
    /// The adapter's transport policy refuses this address (e.g. TLS not available).
    #[error("address refused by transport policy")]
    Refused,
    #[error("response body too large (> {max} bytes)")]
    TooLarge { max: usize },
    /// Connected, but the exchange failed midway (reset, malformed HTTP, TLS error).
    #[error("http transport failure")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// The body of a streamed response.
pub trait ResponseBody: Send + 'static {
    /// Next bytes of the body (1..=`MAX_CHUNK_BYTES`), `Ok(None)` at the end. Cancel-safe: a
    /// dropped call loses no bytes. No deadline of its own: the core wraps it.
    fn chunk(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, HttpError>> + Send;
}

/// A body that is never produced, for `GatewayHttp` doubles without streams.
impl ResponseBody for std::convert::Infallible {
    async fn chunk(&mut self) -> Result<Option<Vec<u8>>, HttpError> {
        match *self {}
    }
}

/// Status + streamed body of an `open_stream` call.
pub struct StreamReply<B> {
    pub status: u16,
    pub body: B,
}

/// Status only: the body is a live connection.
impl<B> fmt::Debug for StreamReply<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StreamReply({})", self.status)
    }
}

pub trait GatewayHttp: Send + Sync + 'static {
    type Body: ResponseBody;

    /// One request/response exchange. Method, path and `Content-Type: application/json` (only
    /// when there is a body) come from `call`; the body is wiped after sending. The token goes
    /// out as `Authorization: Bearer <token>`.
    ///
    /// `Refused`, without touching the network, when `call.is_stream()` (streams have their own
    /// entry point), when `call.authenticated()` and `token` is `None`, when `token` is given on
    /// an unauthenticated call, or when the token is empty or has a byte outside `0x21..=0x7E`
    /// (it could break the header line).
    fn request(
        &self,
        addr: &GatewayAddr,
        call: &HttpCall,
        token: Option<&SecretString>,
        body: Option<Zeroizing<Vec<u8>>>,
    ) -> impl Future<Output = Result<HttpReply, HttpError>> + Send;

    /// Opens a streamed response (`call.is_stream()`): returns once the response head arrived.
    /// Sends `Accept: text/event-stream` for `Events`, `application/octet-stream` for `Speech`,
    /// and `Last-Event-ID` when given. `Refused`, without touching the network, unless
    /// [`stream_allowed`] holds, or when the transport policy refuses `addr`.
    fn open_stream(
        &self,
        addr: &GatewayAddr,
        call: &HttpCall,
        token: &SecretString,
        last_event_id: Option<&str>,
        body: Option<Zeroizing<Vec<u8>>>,
    ) -> impl Future<Output = Result<StreamReply<Self::Body>, HttpError>> + Send;
}

/// Whether `token` may be written into a header line (and so be sent at all).
pub fn token_is_sendable(token: &SecretString) -> bool {
    let t = token.expose_secret().as_bytes();
    !t.is_empty() && t.iter().all(|b| (0x21..=0x7E).contains(b))
}

/// The checks every adapter and fake applies before touching the network.
pub fn request_allowed(call: &HttpCall, token: Option<&SecretString>) -> bool {
    !call.is_stream()
        && match token {
            Some(t) => call.authenticated() && token_is_sendable(t),
            None => !call.authenticated(),
        }
}

/// The checks every adapter and fake applies before opening a stream: a stream call, a
/// sendable token and, when given, a non-empty `Last-Event-ID` of at most `MAX_LAST_EVENT_ID`
/// bytes in `0x21..=0x7E`.
pub fn stream_allowed(call: &HttpCall, token: &SecretString, last_event_id: Option<&str>) -> bool {
    call.is_stream()
        && call.authenticated()
        && token_is_sendable(token)
        && last_event_id.is_none_or(|id| {
            !id.is_empty()
                && id.len() <= MAX_LAST_EVENT_ID
                && id.bytes().all(|b| (0x21..=0x7E).contains(&b))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_checks() {
        let ev = HttpCall::Events { audio: true };
        let t = SecretString::from("t");
        let ok = "x".repeat(MAX_LAST_EVENT_ID);
        let long = "x".repeat(MAX_LAST_EVENT_ID + 1);
        for (call, tok, id, want) in [
            (&ev, "t", None, true),
            (&ev, "t", Some(ok.as_str()), true),
            (&ev, "t", Some("41"), true),
            (&ev, "t", Some(long.as_str()), false),
            (&ev, "t", Some("a\nb"), false),
            (&ev, "t", Some("a b"), false),
            (&ev, "t", Some(""), false),
            (&ev, "t", Some("ñ"), false),
            (&ev, "a b", None, false),
            (&ev, "", None, false),
            (&HttpCall::Me, "t", None, false),
        ] {
            assert_eq!(
                stream_allowed(call, &SecretString::from(tok), id),
                want,
                "{call:?} {id:?}"
            );
        }
        assert!(!request_allowed(&ev, Some(&t)));
        assert_eq!(
            format!(
                "{:?}",
                StreamReply {
                    status: 503,
                    body: ()
                }
            ),
            "StreamReply(503)"
        );
    }

    #[test]
    fn path_id_accepts_only_the_safe_charset() {
        for ok in ["c-1", "agent:main:abc", "A_b.9", &"x".repeat(128)] {
            assert!(PathId::new(ok).is_some(), "{ok}");
        }
        for bad in [
            ".",
            "..",
            "",
            "../x",
            "a/b",
            "a b",
            "%2F",
            "a?b",
            "a#b",
            "ñ",
            &"x".repeat(129),
            "a\r\nb",
        ] {
            assert!(PathId::new(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn path_id_dots_only_rejected_when_exact() {
        for ok in ["a.b", "..x", "x..", ".a"] {
            assert!(PathId::new(ok).is_some(), "{ok}");
        }
    }

    #[test]
    fn calls_map_to_method_path_and_limits() {
        let id = || PathId::new("c1").unwrap();
        let m = || PathId::new("m9").unwrap();
        // (call, method, path, authenticated, is_stream)
        let cases = [
            (
                HttpCall::WellKnown,
                "GET",
                "/.well-known/fogar-gateway",
                false,
                false,
            ),
            (
                HttpCall::WellKnownLegacy,
                "GET",
                "/.well-known/jota-gateway",
                false,
                false,
            ),
            (HttpCall::Pair, "POST", "/pair", false, false),
            (HttpCall::Login, "POST", "/auth/login", false, false),
            (HttpCall::Logout, "POST", "/auth/logout", true, false),
            (HttpCall::Me, "GET", "/auth/me", true, false),
            (HttpCall::UpdateMe, "PATCH", "/auth/me", true, false),
            (
                HttpCall::ListConversations,
                "GET",
                "/conversations",
                true,
                false,
            ),
            (
                HttpCall::ListAgents,
                "GET",
                "/conversations/agents",
                true,
                false,
            ),
            (
                HttpCall::GetConversation { id: id() },
                "GET",
                "/conversations/c1",
                true,
                false,
            ),
            (
                HttpCall::GetMessages {
                    id: id(),
                    before: None,
                },
                "GET",
                "/conversations/c1/messages",
                true,
                false,
            ),
            (
                HttpCall::GetMessages {
                    id: id(),
                    before: Some(m()),
                },
                "GET",
                "/conversations/c1/messages?before=m9",
                true,
                false,
            ),
            (
                HttpCall::CreateConversation,
                "POST",
                "/conversations",
                true,
                false,
            ),
            (
                HttpCall::PostMessage { id: id() },
                "POST",
                "/conversations/c1/messages",
                true,
                false,
            ),
            (
                HttpCall::Withdraw {
                    id: id(),
                    message: m(),
                },
                "DELETE",
                "/conversations/c1/messages/m9",
                true,
                false,
            ),
            (
                HttpCall::Cancel { id: id() },
                "POST",
                "/conversations/c1/cancel",
                true,
                false,
            ),
            (
                HttpCall::Events { audio: false },
                "GET",
                "/events",
                true,
                true,
            ),
            (
                HttpCall::Events { audio: true },
                "GET",
                "/events?audio=1",
                true,
                true,
            ),
            (
                HttpCall::Speech {
                    id: id(),
                    message: m(),
                },
                "POST",
                "/conversations/c1/messages/m9/speech",
                true,
                true,
            ),
        ];
        for (call, method, path, auth, stream) in cases {
            assert_eq!(
                (
                    call.method(),
                    call.path(),
                    call.authenticated(),
                    call.is_stream()
                ),
                (method, path.to_string(), auth, stream),
                "{call:?}"
            );
            let list = matches!(
                call,
                HttpCall::ListConversations | HttpCall::ListAgents | HttpCall::GetMessages { .. }
            );
            let max = if list {
                MAX_LIST_BODY_BYTES
            } else {
                MAX_BODY_BYTES
            };
            assert_eq!(call.max_body(), max, "{call:?}");
        }
    }
}
