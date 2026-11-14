//! Shared dialer for the gateway adapters (HTTP + WS): resolve ONCE, apply the clear-text
//! policy to EVERY resolved address, then connect to exactly those IPs (no second lookup, so
//! no TOCTOU between the policy check and the connect). `Scheme::Tls` wraps the socket in
//! rustls; the certificate is verified for the host the person typed (NET-1), so the
//! clear-text policy does not apply to it.

use fogar_ports::gateway::{GatewayAddr, Scheme, cleartext_allowed};
use rustls::{ClientConfig, pki_types::ServerName};
use std::{future::Future, io, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};

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

/// TLS client configuration: ring provider, safe default versions (1.2 + 1.3), certificates
/// verified by the OS trust store (rustls-platform-verifier). `None` when the platform
/// verifier cannot be built: `https`/`wss` are then refused, never downgraded.
pub(crate) fn platform_tls() -> Option<Arc<ClientConfig>> {
    use rustls_platform_verifier::BuilderVerifierExt;
    ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .ok()?
        .with_platform_verifier()
        .ok()
        .map(|b| Arc::new(b.with_no_client_auth()))
}

/// Byte stream to the gateway: plain TCP or TLS over TCP.
pub(crate) trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}
pub(crate) type Conn = Box<dyn Io>;

#[derive(Debug)]
pub(crate) enum DialError {
    /// Transport policy: TLS unavailable, or clear text to a non-private address.
    Refused,
    /// Name did not resolve or no address accepted the connection.
    Unreachable(io::Error),
    /// Connected, but the TLS handshake failed (bad certificate, wrong name, protocol error).
    Tls(io::Error),
}

/// Resolves `addr` once and connects to the first resolved address that answers within
/// `per_attempt`; for `Scheme::Tls` then runs the handshake (also within `per_attempt`).
pub(crate) async fn dial<R: Resolver>(
    resolver: &R,
    tls: Option<&Arc<ClientConfig>>,
    addr: &GatewayAddr,
    per_attempt: Duration,
) -> Result<Conn, DialError> {
    let tls = match addr.scheme() {
        Scheme::Tls => {
            let cfg = tls.ok_or(DialError::Refused)?;
            let name =
                ServerName::try_from(addr.host().to_owned()).map_err(|_| DialError::Refused)?;
            Some((cfg, name))
        }
        Scheme::Plain => None,
    };
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
    if tls.is_none() && ips.iter().any(|a| !cleartext_allowed(a.ip())) {
        return Err(DialError::Refused);
    }
    let tcp = connect_any(ips, per_attempt).await?;
    let Some((cfg, name)) = tls else {
        return Ok(Box::new(tcp));
    };
    let handshake = tokio_rustls::TlsConnector::from(cfg.clone()).connect(name, tcp);
    match tokio::time::timeout(per_attempt, handshake).await {
        Ok(Ok(s)) => Ok(Box::new(s)),
        Ok(Err(e)) => Err(DialError::Tls(e)),
        Err(_) => Err(DialError::Tls(io::ErrorKind::TimedOut.into())),
    }
}

async fn connect_any(ips: Vec<SocketAddr>, per_attempt: Duration) -> Result<TcpStream, DialError> {
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
