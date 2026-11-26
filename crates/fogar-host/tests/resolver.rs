//! NET-1 against a hostile resolver: a LAN-looking name that resolves to a public IP (alone or
//! mixed with a private one) is refused by both adapters before a byte is sent; a name that
//! resolves to a private IP is connected to THAT address (no second lookup).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use fogar_host::{HttpConfig, HttpGateway, Resolver, WsConfig, WsGatewayLink};
use fogar_ports::{
    gateway::{GatewayAddr, GatewayLink, LinkError, WsRoute},
    gateway_http::{GatewayHttp, HttpCall, HttpError},
};
use fogar_testkit::{HttpRoutes, MockConfig, MockGateway, MockHttpServer, run_async};
use std::{
    io,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

/// Answers every name with `ips` and counts lookups.
#[derive(Debug, Clone)]
struct Fixed {
    ips: Vec<IpAddr>,
    lookups: Arc<AtomicUsize>,
}

impl Fixed {
    fn new(ips: &[&str]) -> Self {
        Self {
            ips: ips.iter().map(|s| s.parse().unwrap()).collect(),
            lookups: Arc::default(),
        }
    }
}

impl Resolver for Fixed {
    async fn resolve(&self, _host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .ips
            .iter()
            .map(|ip| SocketAddr::new(*ip, port))
            .collect())
    }
}

const HOSTILE: [&[&str]; 3] = [
    &["203.0.113.7"],
    &["192.168.1.19", "203.0.113.7"],
    &["::ffff:8.8.8.8"],
];

async fn bounded<F: Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(1), f)
        .await
        .expect("refusal must be immediate")
}

#[test]
fn http_refuses_a_lan_name_that_resolves_public() {
    run_async(async {
        let addr = GatewayAddr::parse("green-house:8004").unwrap();
        for ips in HOSTILE {
            let h = HttpGateway::with_resolver(HttpConfig::default(), Fixed::new(ips));
            let r = bounded(h.request(&addr, &HttpCall::WellKnown, None, None)).await;
            assert!(matches!(r, Err(HttpError::Refused)), "{ips:?}");
            let r = bounded(h.request(&addr, &HttpCall::Login, None, Some(b"{}".to_vec().into())))
                .await;
            assert!(matches!(r, Err(HttpError::Refused)), "{ips:?}");
        }
    })
}

#[test]
fn stream_refuses_a_lan_name_that_resolves_public() {
    run_async(async {
        let addr = GatewayAddr::parse("green-house:8004").unwrap();
        let tok = secrecy::SecretString::from("t");
        for ips in HOSTILE {
            let h = HttpGateway::with_resolver(HttpConfig::default(), Fixed::new(ips));
            let call = HttpCall::Events { audio: false };
            let r = bounded(h.open_stream(&addr, &call, &tok, None, None)).await;
            assert!(matches!(r, Err(HttpError::Refused)), "{ips:?}");
        }
    })
}

#[test]
fn ws_refuses_a_lan_name_that_resolves_public() {
    run_async(async {
        let addr = GatewayAddr::parse("green-house:8004").unwrap();
        for ips in HOSTILE {
            let link = WsGatewayLink::with_resolver(WsConfig::default(), Fixed::new(ips));
            let r = bounded(link.connect(&addr, WsRoute::Stream)).await;
            assert!(matches!(r, Err(LinkError::NotConfigured)), "{ips:?}");
            let r = bounded(link.connect(&addr, WsRoute::Dictation)).await;
            assert!(
                matches!(r, Err(LinkError::NotConfigured)),
                "dictation {ips:?}"
            );
        }
    })
}

#[test]
fn http_connects_to_the_resolved_private_address() {
    run_async(async {
        let s = MockHttpServer::start(HttpRoutes::fogar()).await.unwrap();
        let addr = GatewayAddr::parse(&format!("green-house:{}", s.addr().port())).unwrap();
        let res = Fixed::new(&["127.0.0.1"]);
        let h = HttpGateway::with_resolver(HttpConfig::default(), res.clone());
        let r = h
            .request(&addr, &HttpCall::WellKnown, None, None)
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(res.lookups.load(Ordering::SeqCst), 1);
        let seen = s.routes().seen();
        assert_eq!(seen[0].path, "/.well-known/fogar-gateway");
        // Unresolvable name: Unreachable, not a panic or a hang.
        let none = HttpGateway::with_resolver(HttpConfig::default(), Fixed::new(&[]));
        assert!(matches!(
            none.request(&addr, &HttpCall::WellKnown, None, None).await,
            Err(HttpError::Unreachable)
        ));
    })
}

#[test]
fn ws_connects_to_the_resolved_private_address() {
    run_async(async {
        let gw = MockGateway::start(MockConfig::default()).await.unwrap();
        let addr = GatewayAddr::parse(&format!("green-house:{}", gw.addr().port())).unwrap();
        let res = Fixed::new(&["127.0.0.1"]);
        let link = WsGatewayLink::with_resolver(WsConfig::default(), res.clone());
        let mut s = link.connect(&addr, WsRoute::Stream).await.unwrap();
        assert_eq!(res.lookups.load(Ordering::SeqCst), 1);
        fogar_ports::gateway::LinkSession::close(&mut s, 1000, "bye")
            .await
            .unwrap();
    })
}

/// Option B (NV-1): a call carrying a secret never goes in clear text to a LAN address, even
/// when the first resolved address is loopback. The mock on 127.0.0.1 would answer, so "no
/// request seen" proves no byte left.
#[test]
fn secret_dial_refuses_lan() {
    run_async(async {
        let s = MockHttpServer::start(HttpRoutes::fogar()).await.unwrap();
        let addr = GatewayAddr::parse(&format!("green-house:{}", s.addr().port())).unwrap();
        for ips in [&["192.168.1.19"][..], &["127.0.0.1", "192.168.1.19"]] {
            let h = HttpGateway::with_resolver(HttpConfig::default(), Fixed::new(ips));
            let body = Some(br#"{"username":"u","password":"p"}"#.to_vec().into());
            let r = bounded(h.request(&addr, &HttpCall::Login, None, body)).await;
            assert!(matches!(r, Err(HttpError::Refused)), "{ips:?}");
            let tok = secrecy::SecretString::from("t");
            let r = bounded(h.request(&addr, &HttpCall::Logout, Some(&tok), None)).await;
            assert!(matches!(r, Err(HttpError::Refused)), "{ips:?}");
            let call = HttpCall::Events { audio: false };
            let r = bounded(h.open_stream(&addr, &call, &tok, None, None)).await;
            assert!(matches!(r, Err(HttpError::Refused)), "{ips:?}");
        }
        assert!(
            s.routes().seen().is_empty(),
            "no request reached the gateway"
        );
        let gw = MockGateway::start(MockConfig::default()).await.unwrap();
        let addr = GatewayAddr::parse(&format!("green-house:{}", gw.addr().port())).unwrap();
        let link = WsGatewayLink::with_resolver(
            WsConfig::default(),
            Fixed::new(&["127.0.0.1", "192.168.1.19"]),
        );
        let r = bounded(link.connect(&addr, WsRoute::Stream)).await;
        assert!(matches!(r, Err(LinkError::NotConfigured)));
    })
}

/// Discovery carries no secret: a LAN gateway is still found (the loopback address answers
/// first, so the test never dials the LAN one).
#[test]
fn well_known_still_allowed_on_lan() {
    run_async(async {
        let s = MockHttpServer::start(HttpRoutes::fogar()).await.unwrap();
        let addr = GatewayAddr::parse(&format!("green-house:{}", s.addr().port())).unwrap();
        let h = HttpGateway::with_resolver(
            HttpConfig::default(),
            Fixed::new(&["127.0.0.1", "192.168.1.19"]),
        );
        for call in [HttpCall::WellKnown, HttpCall::WellKnownLegacy] {
            let r = bounded(h.request(&addr, &call, None, None)).await;
            assert!(!matches!(r, Err(HttpError::Refused)), "{call:?}");
        }
        assert_eq!(s.routes().seen().len(), 2);
    })
}

/// A name whose first address is dead still reaches the gateway at the next one, well inside
/// the core's 1-2 s probe deadline.
#[test]
fn http_falls_back_to_the_next_resolved_address() {
    run_async(async {
        let s = MockHttpServer::start(HttpRoutes::fogar()).await.unwrap();
        let addr = GatewayAddr::parse(&format!("green-house:{}", s.addr().port())).unwrap();
        // The mock listens on 127.0.0.1 only: ::1 refuses, then 127.0.0.1 answers.
        let h =
            HttpGateway::with_resolver(HttpConfig::default(), Fixed::new(&["::1", "127.0.0.1"]));
        let r = tokio::time::timeout(
            Duration::from_secs(2),
            h.request(&addr, &HttpCall::WellKnown, None, None),
        )
        .await
        .expect("fallback within the probe deadline")
        .unwrap();
        assert_eq!(r.status, 200);
    })
}
