use fogar_ports::{
    Support,
    events::{EventSink, EventStream, Pushed, event_channel},
    system_probe::{Battery, Memory, ProbeCaps, ProbeError, ProbeEvent, SystemProbe, SystemSnapshot, Thermal},
};
use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Debug)]
struct ProbeState {
    snapshot: SystemSnapshot,
    fail_next: Option<ProbeError>,
    sink: Option<EventSink<ProbeEvent>>,
    calls: u64,
}

/// Cloneable handle: give one clone to the core, keep one in the test to script it.
#[derive(Debug, Clone)]
pub struct FakeSystemProbe {
    caps: ProbeCaps,
    state: Arc<Mutex<ProbeState>>,
}

impl FakeSystemProbe {
    /// Everything `Native`; snapshot fully populated with benign values.
    pub fn healthy() -> Self {
        let mut caps = ProbeCaps::NONE;
        caps.load = Support::Native;
        caps.memory = Support::Native;
        caps.thermal = Support::Native;
        caps.battery = Support::Native;
        caps.idle = Support::Native;
        caps.change_events = Support::Native;
        let snapshot = SystemSnapshot::new()
            .with_load(0.5)
            .with_memory(Memory { total_bytes: 8 << 30, available_bytes: 4 << 30 })
            .with_thermal(Thermal::Nominal)
            .with_battery(Battery { charge: 0.8, on_external_power: true, low_power_mode: Some(false) })
            .with_idle(Duration::from_secs(0));
        Self::with(caps, snapshot)
    }

    /// Nothing supported: models a minimal platform. The core must still work.
    pub fn minimal() -> Self {
        Self::with(ProbeCaps::NONE, SystemSnapshot::new())
    }

    pub fn with(caps: ProbeCaps, snapshot: SystemSnapshot) -> Self {
        Self {
            caps,
            state: Arc::new(Mutex::new(ProbeState { snapshot, fail_next: None, sink: None, calls: 0 })),
        }
    }

    pub fn set_snapshot(&self, s: SystemSnapshot) {
        self.state.lock().unwrap().snapshot = s;
    }
    pub fn fail_next(&self, e: ProbeError) {
        self.state.lock().unwrap().fail_next = Some(e);
    }
    /// Push an OS event as the real adapter would from its callback thread.
    pub fn emit(&self, ev: ProbeEvent) -> Option<Pushed> {
        self.state.lock().unwrap().sink.as_ref().map(|s| s.push(ev))
    }
    pub fn snapshot_calls(&self) -> u64 {
        self.state.lock().unwrap().calls
    }
}

impl SystemProbe for FakeSystemProbe {
    fn capabilities(&self) -> ProbeCaps {
        self.caps
    }
    fn snapshot(&self) -> Result<SystemSnapshot, ProbeError> {
        let mut st = self.state.lock().unwrap();
        st.calls += 1;
        if let Some(e) = st.fail_next.take() {
            return Err(e);
        }
        Ok(st.snapshot)
    }
    fn subscribe(&self, capacity: NonZeroUsize) -> Result<EventStream<ProbeEvent>, ProbeError> {
        if !self.caps.change_events.is_available() {
            return Err(ProbeError::Unsupported("change_events"));
        }
        let (tx, rx) = event_channel(capacity);
        self.state.lock().unwrap().sink = Some(tx);
        Ok(rx)
    }
}
