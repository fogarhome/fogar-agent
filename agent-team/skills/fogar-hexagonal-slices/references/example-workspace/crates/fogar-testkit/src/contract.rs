//! Contract suites: the executable definition of each port.
//!
//! Every adapter crate (macos / windows / linux / fake) runs the suite from its own
//! `tests/contract.rs` with one macro line. A port change = change the suite first
//! (contract-first), then fix the adapters until their CI is green again.

pub mod system_probe {
    use fogar_ports::system_probe::{ProbeError, SystemProbe};
    use std::{
        num::NonZeroUsize,
        time::{Duration, Instant},
    };

    /// Perf budget for `snapshot()`. Energy matters: the core polls this for days.
    pub const SNAPSHOT_BUDGET: Duration = Duration::from_millis(5);

    pub fn caps_are_stable(p: &dyn SystemProbe) {
        let a = p.capabilities();
        for _ in 0..10 {
            assert_eq!(
                a,
                p.capabilities(),
                "capabilities() must not change over the adapter's lifetime"
            );
        }
    }

    pub fn snapshot_matches_caps(p: &dyn SystemProbe) {
        let c = p.capabilities();
        let s = p
            .snapshot()
            .expect("snapshot() must succeed on a healthy system");
        // Unsupported => field must be None. (Supported may still be None, e.g. no battery.)
        if !c.load.is_available() {
            assert!(
                s.load_1m.is_none(),
                "load reported but caps say Unsupported"
            );
        }
        if !c.memory.is_available() {
            assert!(s.memory.is_none());
        }
        if !c.thermal.is_available() {
            assert!(s.thermal.is_none());
        }
        if !c.battery.is_available() {
            assert!(s.battery.is_none());
        }
        if !c.idle.is_available() {
            assert!(s.idle_for.is_none());
        }
    }

    pub fn values_are_sane(p: &dyn SystemProbe) {
        let s = p.snapshot().unwrap();
        if let Some(l) = s.load_1m {
            assert!(l.is_finite() && l >= 0.0, "load {l}");
        }
        if let Some(m) = s.memory {
            assert!(
                m.total_bytes > 0 && m.available_bytes <= m.total_bytes,
                "{m:?}"
            );
        }
        if let Some(b) = s.battery {
            assert!((0.0..=1.0).contains(&b.charge), "{b:?}");
        }
    }

    pub fn snapshot_is_cheap(p: &dyn SystemProbe) {
        let _ = p.snapshot(); // warm-up (first call may open handles)
        let n = 50;
        let t = Instant::now();
        for _ in 0..n {
            p.snapshot().unwrap();
        }
        let per_call = t.elapsed() / n;
        assert!(
            per_call < SNAPSHOT_BUDGET,
            "snapshot() too slow: {per_call:?}"
        );
    }

    pub fn subscribe_matches_caps(p: &dyn SystemProbe) {
        let r = p.subscribe(NonZeroUsize::new(4).unwrap());
        match (p.capabilities().change_events.is_available(), r) {
            (true, Ok(stream)) => drop(stream), // dropping the stream must not panic the adapter
            (false, Err(ProbeError::Unsupported(_))) => {}
            (true, Err(e)) => panic!("caps claim change_events but subscribe failed: {e}"),
            (false, Ok(_)) => panic!("subscribe succeeded but caps say Unsupported"),
            (false, Err(e)) => {
                panic!("unsupported subscribe must return ProbeError::Unsupported, got {e}")
            }
        }
        // Adapter must still work after the subscriber went away.
        p.snapshot().unwrap();
    }

    /// Generates one `#[test]` per contract clause, so failures are reported by name.
    /// Usage (in an adapter's tests/contract.rs):
    /// `fogar_testkit::system_probe_contract!(my_adapter::Probe::new().unwrap());`
    #[macro_export]
    macro_rules! system_probe_contract {
        ($make:expr) => {
            mod system_probe_contract {
                #[allow(unused_imports)]
                use super::*;
                use $crate::contract::system_probe as c;
                #[test]
                fn caps_are_stable() {
                    c::caps_are_stable(&$make)
                }
                #[test]
                fn snapshot_matches_caps() {
                    c::snapshot_matches_caps(&$make)
                }
                #[test]
                fn values_are_sane() {
                    c::values_are_sane(&$make)
                }
                #[test]
                fn snapshot_is_cheap() {
                    c::snapshot_is_cheap(&$make)
                }
                #[test]
                fn subscribe_matches_caps() {
                    c::subscribe_matches_caps(&$make)
                }
            }
        };
    }
}

pub mod uplink {
    use crate::block_on_ready;
    use fogar_ports::uplink::{Uplink, UplinkError};

    /// For adapters whose futures complete without a runtime (fakes). Real adapters
    /// call the `*_async` variants from inside their runtime's test attribute.
    pub fn rejects_oversized<U: Uplink>(u: &U) {
        block_on_ready(rejects_oversized_async(u))
    }

    pub async fn rejects_oversized_async<U: Uplink>(u: &U) {
        let too_big = vec![0u8; u.max_frame() + 1];
        match u.send(too_big).await {
            Err(UplinkError::TooLarge { .. }) => {}
            other => panic!("oversized frame must be TooLarge, got {other:?}"),
        }
    }

    /// Dropping an in-flight send (timeout / shutdown) must not poison the adapter.
    pub async fn survives_cancelled_send_async<U: Uplink>(u: &U) {
        drop(u.send(vec![1, 2, 3])); // created, never polled, dropped
        let _ = u.send(vec![4]).await; // must not panic or hang
    }
}
