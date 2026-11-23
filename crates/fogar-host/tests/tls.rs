//! `https://` / `wss://` end to end: a local rustls server (test CA in `tests/tls/`, see
//! `gen.sh`) terminates TLS in front of the testkit mocks, and BOTH contract suites run through
//! it unchanged. Negative cases: an unknown CA and a name the certificate does not cover fail
//! the handshake; TLS is never downgraded to clear text.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use fogar_host::{HttpConfig, HttpGateway, Resolver, WsConfig, WsGatewayLink};
use fogar_ports::{
    gateway::{GatewayAddr, GatewayLink, LinkError},
    gateway_http::{GatewayHttp, HttpCall, HttpError},
};
use fogar_testkit::{HttpRoutes, MockConfig, MockGateway, MockHttpServer, run_async};
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use std::{io, net::SocketAddr, sync::Arc};
use tokio::{net::TcpListener, task::JoinHandle};

const CA: &[u8] = include_bytes!("tls/ca.pem");
const OTHER_CA: &[u8] = include_bytes!("tls/other-ca.pem");
const LEAF: &[u8] = include_bytes!("tls/leaf.pem");
const LEAF_KEY: &[u8] = include_bytes!("tls/leaf.key");

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A verifying client config that trusts only `ca`.
fn client(ca: &[u8]) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(ca).unwrap())
        .unwrap();
    Arc::new(
        ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// TLS-terminating proxy on 127.0.0.1 forwarding to `backend`. Stops when dropped.
struct TlsProxy {
    port: u16,
    task: JoinHandle<()>,
}

impl Drop for TlsProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TlsProxy {
    async fn start(backend: GatewayAddr) -> Self {
        let cfg = ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from_pem_slice(LEAF).unwrap()],
                PrivateKeyDer::from_pem_slice(LEAF_KEY).unwrap(),
            )
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (acceptor, backend) = (acceptor.clone(), backend.authority());
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let Ok(mut up) = tokio::net::TcpStream::connect(backend).await else {
                        return;
                    };
                    let _ = tokio::io::copy_bidirectional(&mut tls, &mut up).await;
                });
            }
        });
        Self { port, task }
    }
    fn addr(&self) -> GatewayAddr {
        GatewayAddr::parse(&format!("https://127.0.0.1:{}", self.port)).unwrap()
    }
}

/// `MockHttpServer` behind TLS, shaped like the contract suite expects.
struct HttpsServer {
    proxy: TlsProxy,
    inner: MockHttpServer,
}

impl HttpsServer {
    async fn start() -> Self {
        let inner = MockHttpServer::start(HttpRoutes::new()).await.unwrap();
        let proxy = TlsProxy::start(inner.addr()).await;
        Self { proxy, inner }
    }
    fn addr(&self) -> GatewayAddr {
        self.proxy.addr()
    }
    fn routes(&self) -> HttpRoutes {
        self.inner.routes()
    }
    /// A closed port: Unreachable before any TLS.
    fn dead_addr(&self) -> GatewayAddr {
        GatewayAddr::parse(&format!("https://{}", self.inner.dead_addr().authority())).unwrap()
    }
}

/// `MockGateway` behind TLS.
struct WssGateway {
    proxy: TlsProxy,
    inner: MockGateway,
}

impl WssGateway {
    async fn start() -> Self {
        let inner = MockGateway::start(MockConfig::default()).await.unwrap();
        let proxy = TlsProxy::start(inner.addr()).await;
        Self { proxy, inner }
    }
    fn addr(&self) -> GatewayAddr {
        GatewayAddr::parse(&format!("wss://127.0.0.1:{}", self.proxy.port)).unwrap()
    }
    fn control(&self, cmd: fogar_testkit::gateway::Ctl) -> usize {
        self.inner.control(cmd)
    }
}

fogar_testkit::gateway_http_contract!({
    let s = HttpsServer::start().await;
    let h = HttpGateway::new(HttpConfig::default()).with_tls(client(CA));
    (h, s)
});

fogar_testkit::gateway_link_contract!({
    let gw = WssGateway::start().await;
    let link = WsGatewayLink::new(WsConfig::default()).with_tls(client(CA));
    (link, gw)
});

/// Real-time tests: a stalled handshake fails the test instead of hanging CI.
async fn bounded<F: Future>(f: F) -> F::Output {
    tokio::time::timeout(std::time::Duration::from_secs(15), f)
        .await
        .expect("TLS test timed out")
}

/// Resolves every name to 127.0.0.1.
#[derive(Debug, Clone, Copy)]
struct Loopback;

impl Resolver for Loopback {
    async fn resolve(&self, _host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
    }
}

#[test]
fn unknown_ca_fails_the_handshake() {
    run_async(bounded(async {
        let s = HttpsServer::start().await;
        let h = HttpGateway::new(HttpConfig::default()).with_tls(client(OTHER_CA));
        assert!(matches!(
            h.request(&s.addr(), &HttpCall::WellKnown, None, None).await,
            Err(HttpError::Transport(_))
        ));
        assert!(
            s.routes().seen().is_empty(),
            "no request crosses a bad handshake"
        );
        let gw = WssGateway::start().await;
        let link = WsGatewayLink::new(WsConfig::default()).with_tls(client(OTHER_CA));
        assert!(matches!(
            link.connect(&gw.addr()).await,
            Err(LinkError::Transport(_))
        ));
    }))
}

#[test]
fn name_not_in_the_certificate_fails_the_handshake() {
    run_async(bounded(async {
        let s = HttpsServer::start().await;
        let port = s.proxy.port;
        // Same server, reached by a name the certificate does not cover.
        let wrong = GatewayAddr::parse(&format!("https://green-house.example:{port}")).unwrap();
        let h = HttpGateway::with_resolver(HttpConfig::default(), Loopback).with_tls(client(CA));
        assert!(matches!(
            h.request(&wrong, &HttpCall::WellKnown, None, None).await,
            Err(HttpError::Transport(_))
        ));
        // ...and by the name it does cover, through the same resolver path.
        let right = GatewayAddr::parse(&format!("https://localhost:{port}")).unwrap();
        s.routes()
            .set("GET", "/.well-known/fogar-gateway", 200, "{}");
        let r = h
            .request(&right, &HttpCall::WellKnown, None, None)
            .await
            .unwrap();
        assert_eq!(r.status, 200);

        let gw = WssGateway::start().await;
        let wrong =
            GatewayAddr::parse(&format!("wss://green-house.example:{}", gw.proxy.port)).unwrap();
        let link = WsGatewayLink::with_resolver(WsConfig::default(), Loopback).with_tls(client(CA));
        assert!(matches!(
            link.connect(&wrong).await,
            Err(LinkError::Transport(_))
        ));
    }))
}

/// A TLS address never falls back to clear text: plain HTTP on the TLS port is a handshake
/// failure, not a reply.
#[test]
fn tls_to_a_plain_server_is_not_downgraded() {
    run_async(bounded(async {
        let plain = MockHttpServer::start(HttpRoutes::fogar("gw"))
            .await
            .unwrap();
        let addr = GatewayAddr::parse(&format!("https://{}", plain.addr().authority())).unwrap();
        let h = HttpGateway::new(HttpConfig::default()).with_tls(client(CA));
        assert!(matches!(
            h.request(&addr, &HttpCall::WellKnown, None, None).await,
            Err(HttpError::Transport(_))
        ));
    }))
}

/// The production constructor builds the platform verifier (OS trust store): it must exist on
/// every supported OS, or `https`/`wss` would silently be refused.
#[test]
fn platform_verifier_is_available() {
    run_async(bounded(async {
        let h = HttpGateway::new(HttpConfig::default());
        // A closed local port: reaching the connect step proves TLS was not refused up front.
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = GatewayAddr::parse(&format!("https://{}", dead.local_addr().unwrap())).unwrap();
        drop(dead);
        assert!(matches!(
            h.request(&addr, &HttpCall::WellKnown, None, None).await,
            Err(HttpError::Unreachable)
        ));
    }))
}
