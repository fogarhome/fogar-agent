//! Shared dialer for the gateway adapters (HTTP + WS): resolve ONCE, apply the clear-text
//! policy to EVERY resolved address, then connect to exactly those IPs (no second lookup, so
//! no TOCTOU between the policy check and the connect).

use fogar_ports::gateway::{GatewayAddr, Scheme, cleartext_allowed};
use std::{future::Future, io, net::SocketAddr, time::Duration};
use tokio::net::TcpStream;

/// At most this many resolved addresses are checked and tried.
const MAX_ADDRS: usize = 8;

/// Name resolution, injectable so tests can make a LAN-looking name resolve to a public IP.
pub trait Resolver: Send + Sync + 'static {
    fn resolve(
        &self,
        host: &str,
        port: u16,
    ) -> impl Future<Output = io::Result<Vec<SocketAddr>>> + Send;
}

/// The OS resolver (`getaddrinfo` on tokio's blocking pool).
///
/// shortcut: a timed-out lookup keeps its blocking thread until `getaddrinfo` returns, so a
/// stalled DNS plus repeated probes can pile up blocking threads (bounded by the runtime's
/// `max_blocking_threads`); upgrade to a single-flight lookup if S1 measurements show it.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemResolver;

impl Resolver for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(tokio::net::lookup_host((host, port))
            .await?
            .take(MAX_ADDRS)
            .collect())
    }
}

#[derive(Debug)]
pub(crate) enum DialError {
    /// Transport policy: TLS unavailable, or clear text to a non-private address.
    Refused,
    /// Name did not resolve or no address accepted the connection.
    Unreachable(io::Error),
}

/// Resolves `addr` once and connects to the first resolved address that answers within
/// `per_attempt`. `Scheme::Tls` is refused: this build has no TLS.
pub(crate) async fn dial<R: Resolver>(
    resolver: &R,
    addr: &GatewayAddr,
    per_attempt: Duration,
) -> Result<TcpStream, DialError> {
    if addr.scheme() == Scheme::Tls {
        return Err(DialError::Refused);
    }
    let ips = match addr.host().parse::<std::net::IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, addr.port())],
        Err(_) => {
            let mut v =
                tokio::time::timeout(per_attempt, resolver.resolve(addr.host(), addr.port()))
                    .await
                    .map_err(|_| DialError::Unreachable(io::ErrorKind::TimedOut.into()))?
                    .map_err(DialError::Unreachable)?;
            v.truncate(MAX_ADDRS);
            v
        }
    };
    // Refuse the whole call if ANY address is public: a hostile resolver must not get the
    // client key in clear text by mixing one private address with a public one.
    if ips.iter().any(|a| !cleartext_allowed(a.ip())) {
        return Err(DialError::Refused);
    }
    let mut last = io::Error::new(io::ErrorKind::NotFound, "name resolved to no address");
    for ip in ips {
        match tokio::time::timeout(per_attempt, TcpStream::connect(ip)).await {
            Ok(Ok(s)) => {
                // Small request/response exchanges: do not wait for Nagle.
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Ok(Err(e)) => last = e,
            Err(_) => last = io::ErrorKind::TimedOut.into(),
        }
    }
    Err(DialError::Unreachable(last))
}
