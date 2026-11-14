//! Slice acceptance test, entirely in-memory (runs on any host OS, in microseconds).
use fogar_core::governor::{Governor, Policy, TickOutcome, WorkMode, decide};
use fogar_ports::{
    clock::Clock,
    system_probe::{Battery, ProbeError, SystemSnapshot, Thermal},
};
use fogar_testkit::{
    block_on_ready,
    fakes::{FakeSystemProbe, RecordingUplink, VirtualClock},
};
use std::time::Duration;

#[test]
fn decide_is_total_and_degrades_gracefully() {
    let p = Policy::default();
    // Nothing known (minimal platform) -> work normally.
    assert_eq!(decide(&SystemSnapshot::new(), &p), WorkMode::Full);
    assert_eq!(
        decide(&SystemSnapshot::new().with_thermal(Thermal::Critical), &p),
        WorkMode::Paused
    );
    let low = Battery {
        charge: 0.1,
        on_external_power: false,
        low_power_mode: None,
    };
    assert_eq!(
        decide(&SystemSnapshot::new().with_battery(low), &p),
        WorkMode::Reduced
    );
}

#[test]
fn reports_mode_changes_once_and_survives_failures() {
    let probe = FakeSystemProbe::healthy();
    let uplink = RecordingUplink::connected();
    let clock = VirtualClock::default();
    let mut g = Governor::new(
        probe.clone(),
        uplink.clone(),
        clock.clone(),
        Policy::default(),
    );

    assert_eq!(
        block_on_ready(g.tick()),
        TickOutcome::Reported(WorkMode::Full)
    );
    assert_eq!(block_on_ready(g.tick()), TickOutcome::Unchanged);

    probe.set_snapshot(SystemSnapshot::new().with_thermal(Thermal::Critical));
    assert_eq!(
        block_on_ready(g.tick()),
        TickOutcome::Reported(WorkMode::Paused)
    );

    probe.fail_next(ProbeError::Transient("busy".into()));
    assert_eq!(block_on_ready(g.tick()), TickOutcome::Degraded);
    assert_eq!(g.mode(), Some(WorkMode::Paused));

    // Frames on the wire decode with the real codec (proto is part of the slice).
    let frames = uplink.frames();
    assert_eq!(frames.len(), 2);
    match fogar_proto::decode(&frames[1]).unwrap() {
        fogar_proto::Message::Status { mode, .. } => assert_eq!(mode, WorkMode::Paused as u8),
        other => panic!("{other:?}"),
    }
}

#[test]
fn three_day_soak_in_virtual_time_polls_slowly_when_stable() {
    let probe = FakeSystemProbe::healthy();
    let clock = VirtualClock::default();
    let mut g = Governor::new(
        probe.clone(),
        RecordingUplink::connected(),
        clock.clone(),
        Policy::default(),
    );
    let three_days = Duration::from_secs(3 * 24 * 3600);
    while clock.now().0 < three_days {
        let o = block_on_ready(g.tick());
        let d = g.next_interval(&o);
        block_on_ready(clock.sleep(d));
    }
    // Stable system -> idle cadence (60 s) dominates: ~4320 polls, not 51840.
    assert!(
        probe.snapshot_calls() < 4_400,
        "{} polls",
        probe.snapshot_calls()
    );
}
