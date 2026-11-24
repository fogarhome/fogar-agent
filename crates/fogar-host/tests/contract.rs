//! fogar-host adapters pass the same contract suites as the fakes.

use fogar_host::{HttpConfig, HttpGateway, PrngRandom, TokioClock, WsConfig, WsGatewayLink};
use fogar_testkit::{HttpRoutes, MockConfig, MockGateway, MockHttpServer};

fogar_testkit::clock_contract!(TokioClock::new());
fogar_testkit::random_contract!(PrngRandom::new());
fogar_testkit::gateway_link_contract!({
    let gw = MockGateway::start(MockConfig::default())
        .await
        .expect("mock gateway");
    let link = WsGatewayLink::new(WsConfig::default());
    (link, gw)
});
fogar_testkit::gateway_http_contract!({
    let s = MockHttpServer::start(HttpRoutes::new())
        .await
        .expect("mock http server");
    (HttpGateway::new(HttpConfig::default()), s)
});
fogar_testkit::gateway_stream_contract!({
    let s = MockHttpServer::start(HttpRoutes::new())
        .await
        .expect("mock http server");
    (HttpGateway::new(HttpConfig::default()), s)
});

/// A bodyless non-GET request declares `Content-Length: 0` (the mock's `SeenRequest` has no
/// raw headers, so read the head off a bare socket).
#[test]
fn bodyless_non_get_sends_content_length_zero() {
    use fogar_ports::gateway::GatewayAddr;
    use fogar_ports::gateway_http::{GatewayHttp, HttpCall, PathId};
    use tokio::io::AsyncReadExt;
    fogar_testkit::run_async(async {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = GatewayAddr::parse(&format!(
            "127.0.0.1:{}",
            l.local_addr().expect("addr").port()
        ))
        .expect("addr");
        let reader = tokio::spawn(async move {
            let (mut c, _) = l.accept().await.expect("accept");
            let mut got = Vec::new();
            let mut buf = [0u8; 512];
            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = c.read(&mut buf).await.expect("read");
                assert!(n > 0, "closed before the head ended");
                got.extend_from_slice(&buf[..n]);
            }
            String::from_utf8(got).expect("utf8")
        });
        let h = HttpGateway::new(HttpConfig::default());
        let call = HttpCall::Withdraw {
            id: PathId::new("c1").expect("id"),
            message: PathId::new("m9").expect("id"),
        };
        let tok = secrecy::SecretString::from("t");
        // The socket never answers: the call times out or errors; only the head matters.
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            h.request(&addr, &call, Some(&tok), None),
        )
        .await;
        let head = reader.await.expect("reader");
        assert!(head.starts_with("DELETE "), "{head}");
        assert!(head.contains("\r\nContent-Length: 0\r\n"), "{head}");
    });
}

/// The `Accept` header per stream kind (the in-memory fake sees no headers, so not a clause).
#[test]
fn stream_accept_headers() {
    use fogar_ports::gateway_http::{GatewayHttp, HttpCall, PathId};
    fogar_testkit::run_async(async {
        let s = MockHttpServer::start(HttpRoutes::new())
            .await
            .expect("mock http server");
        let h = HttpGateway::new(HttpConfig::default());
        let tok = secrecy::SecretString::from("t");
        let speech = HttpCall::Speech {
            id: PathId::new("c1").expect("id"),
            message: PathId::new("m9").expect("id"),
        };
        for (call, accept) in [
            (HttpCall::Events { audio: true }, "text/event-stream"),
            (speech, "application/octet-stream"),
        ] {
            let r = h
                .open_stream(&s.addr(), &call, &tok, None, None)
                .await
                .expect("open");
            assert_eq!(r.status, 404);
            let seen = s.routes().seen().pop().expect("seen");
            assert_eq!(seen.path, call.path());
            assert_eq!(seen.accept.as_deref(), Some(accept));
        }
    });
}
