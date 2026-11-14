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
//! - The adapter bounds the response body (`MAX_BODY_BYTES`) and never follows redirects (a
//!   redirect could move the key exchange to another host).
//! - Bodies are wiped from memory on drop and never printed: the `/pair` reply carries the
//!   `client_key` exactly once.
//!
//! How Windows/Linux would do this: the same portable HTTP/1.1 client (fogar-host); nothing OS
//! specific, so no capabilities.

use crate::gateway::GatewayAddr;
use secrecy::zeroize::Zeroizing;
use std::{fmt, future::Future};

/// Largest response body an adapter accepts. Both replies are tiny JSON objects.
pub const MAX_BODY_BYTES: usize = 16 * 1024;

/// The two paths the core may call. Closed set: the port cannot be used as a generic client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HttpCall {
    /// `GET <path>` where path is one of `fogar_proto::pairing::WELL_KNOWN_PATHS`.
    WellKnown(&'static str),
    /// `POST /pair` with a JSON body.
    Pair,
}

impl HttpCall {
    pub fn path(self) -> &'static str {
        match self {
            HttpCall::WellKnown(p) => p,
            HttpCall::Pair => "/pair",
        }
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
    /// `call` must be `HttpCall::WellKnown`; `Pair` with a GET is a core bug and returns
    /// `Refused` without touching the network.
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
