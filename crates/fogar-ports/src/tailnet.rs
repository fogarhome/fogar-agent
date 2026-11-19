//! TailnetPeers port: the machines of the user's private overlay network (Tailscale tailnet),
//! where discovery looks for gateways after the saved address and this machine (spec §0 step 3).
//!
//! - ASYNC: the macOS adapter asks the Tailscale app (a child process, ~100 ms). Desugared
//!   `fn -> impl Future + Send`, generic like `GatewayHttp`.
//! - ON DEMAND ONLY: called when the user asks to discover; never polled (idle cost 0).
//! - NOT a select branch: the core wraps the call in its own deadline and drops it on expiry.
//!   Dropping an in-flight call must not leave anything behind (the adapter kills its child).
//! - Answers are data, not trust: every address is checked with [`is_tailnet_ip`] by the
//!   adapter, and the core still probes each one through `GatewayHttp` with its cleartext policy.
//!   A peer is not necessarily the user's own machine (nodes can be shared into a tailnet), so a
//!   gateway found there is only a suggestion: the UI shows its address and the user picks it.
//!
//! How Windows/Linux would do this: the same `tailscale status --json` (CLI on PATH or the
//! LocalAPI socket of `tailscaled`); until then they use the core's `NoTailnet` (Unsupported).

use crate::Support;
use std::{future::Future, net::IpAddr};

/// Most addresses an adapter returns (one per peer). The core probes each one.
pub const MAX_TAILNET_PEERS: usize = 32;

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum TailnetError {
    /// No overlay client on this machine (or this adapter is `Unsupported`).
    #[error("tailnet client unavailable")]
    Unavailable,
    /// Installed, but not connected (logged out, stopped, starting).
    #[error("tailnet not running")]
    NotRunning,
    /// The client answered something we could not read.
    #[error("tailnet answer malformed")]
    Malformed,
}

pub trait TailnetPeers: Send + Sync + 'static {
    /// Fixed for the adapter's lifetime. `Unsupported` ⇒ `peers` returns `Unavailable`
    /// without any I/O.
    fn capabilities(&self) -> Support;
    /// One address per ONLINE peer, never this machine: an IPv4 tailnet address when the peer
    /// has one, else its IPv6 one. No duplicates, every one passes [`is_tailnet_ip`], at most
    /// [`MAX_TAILNET_PEERS`].
    fn peers(&self) -> impl Future<Output = Result<Vec<IpAddr>, TailnetError>> + Send;
}

/// Tailscale's address ranges: CGNAT 100.64.0.0/10 and the ULA prefix fd7a:115c:a1e0::/48.
pub fn is_tailnet_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && (b & 0xc0) == 64
        }
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn tailnet_ranges() {
        let t = |s: &str| is_tailnet_ip(s.parse().unwrap());
        for ok in [
            "100.64.0.1",
            "100.64.0.10",
            "100.127.255.255",
            "fd7a:115c:a1e0::10",
        ] {
            assert!(t(ok), "{ok}");
        }
        for no in [
            "100.63.255.255",
            "100.128.0.0",
            "192.168.1.19",
            "127.0.0.1",
            "fd7a:115c:a1e1::1",
            "::1",
        ] {
            assert!(!t(no), "{no}");
        }
    }
}
