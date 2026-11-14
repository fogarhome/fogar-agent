//! The fakes must satisfy the same contracts as real adapters.
use fogar_testkit::fakes::{FakeSystemProbe, RecordingUplink};

mod healthy {
    use super::*;
    fogar_testkit::system_probe_contract!(FakeSystemProbe::healthy());
}

mod minimal {
    use super::*;
    fogar_testkit::system_probe_contract!(FakeSystemProbe::minimal());
}

#[test]
fn recording_uplink_rejects_oversized() {
    fogar_testkit::contract::uplink::rejects_oversized(&RecordingUplink::connected());
}

#[test]
fn recording_uplink_survives_cancel() {
    fogar_testkit::block_on_ready(
        fogar_testkit::contract::uplink::survives_cancelled_send_async(
            &RecordingUplink::connected(),
        ),
    );
}
