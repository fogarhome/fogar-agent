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
