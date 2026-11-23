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
