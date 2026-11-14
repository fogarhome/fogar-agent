//! Constants duplicated across crates that must agree (ports do not depend on proto).

use fogar_ports::gateway_http::HttpCall;
use fogar_proto::pairing::WELL_KNOWN_PATHS;

#[test]
fn http_call_paths_match_the_protocol() {
    assert_eq!(
        [HttpCall::WellKnown.path(), HttpCall::WellKnownLegacy.path()],
        WELL_KNOWN_PATHS
    );
}
