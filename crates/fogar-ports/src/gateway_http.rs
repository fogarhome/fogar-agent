//! GatewayHttp port: the gateway's two unauthenticated REST calls used before a session exists
//! (client-protocol §0): `GET /.well-known/fogar-gateway` (is this a Fogar?) and `POST /pair`
//! (redeem a 6-digit code for the `client_key`).
//!
//! TRANSPORT ONLY, like `GatewayLink`: status code + body bytes. Paths, request bodies and
//! response parsing live in fogar-proto (`pairing`) + fogar-core, so the adapter cannot drift
//! from the protocol and the fake exercises the real codec.
//!
//! - ASYNC (real network latency), desugared `fn -> impl Future + Send`. Not dyn-compatible: the
//!   core is generic over `H: GatewayHttp`.
//! - NOT select branches: the core wraps each call in its own deadline (1–2 s per probe, spec §0)
//!   and drops it on expiry. Dropping an in-flight call must not leak a socket or poison the
//!   adapter: the next call works.
//! - Transport policy: `Scheme::Plain` only when `gateway::cleartext_allowed` holds for every
//!   resolved socket address (checked before sending a byte; else `Refused`). `Scheme::Tls`
//!   verifies the certificate against the platform roots; no "accept invalid" switch.
//! - One request per connection (`Connection: close`), no pooling: a cancelled call can never
//!   hand a half-read connection to the next one.
//! - The adapter bounds the response body (`MAX_BODY_BYTES`) WHILE reading: a larger or endless
//!   body (lying `Content-Length`, chunked) is `TooLarge` as soon as the limit is crossed, never
//!   buffered first. It never follows redirects (a redirect could move the key exchange to
//!   another host).
//! - Bodies are wiped from memory on drop and never printed: the `/pair` reply carries the
//!   `client_key` exactly once. Adapters read the body into ONE buffer pre-sized from
//!   `Content-Length` (capped at `MAX_BODY_BYTES`), so growth never leaves stray copies.
//!
//! How Windows/Linux would do this: the same portable HTTP/1.1 client (fogar-host); nothing OS
//! specific, so no capabilities.

use crate::gateway::GatewayAddr;
use secrecy::zeroize::Zeroizing;
use std::{fmt, future::Future};

/// Largest response body an adapter accepts. Both replies are tiny JSON objects.
pub const MAX_BODY_BYTES: usize = 16 * 1024;

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

pub trait GatewayHttp: Send + Sync + 'static {
    /// `call` must be a well-known call; `Pair` with a GET is a core bug and returns `Refused`
    /// without touching the network.
    fn get(
        &self,
        addr: &GatewayAddr,
        call: HttpCall,
    ) -> impl Future<Output = Result<HttpReply, HttpError>> + Send;
    /// `POST` with `Content-Type: application/json`. The request body is wiped after sending.
    fn post_json(
        &self,
        addr: &GatewayAddr,
        call: HttpCall,
        body: Zeroizing<Vec<u8>>,
    ) -> impl Future<Output = Result<HttpReply, HttpError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

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
