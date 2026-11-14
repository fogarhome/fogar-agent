//! fogar-host adapters pass the same contract suites as the fakes.

use fogar_host::{PrngRandom, TokioClock, WsConfig, WsGatewayLink};
use fogar_testkit::{MockConfig, MockGateway};

fogar_testkit::clock_contract!(TokioClock::new());
fogar_testkit::random_contract!(PrngRandom::new());
fogar_testkit::gateway_link_contract!({
    let gw = MockGateway::start(MockConfig::default())
        .await
        .expect("mock gateway");
    let link = WsGatewayLink::new(WsConfig::default());
    (link, gw)
});
