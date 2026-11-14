//! Slice 1 ("walking skeleton"): probe system -> decide work mode -> report to gateway.
//!
//! Split: `decide` is a PURE function (exhaustively unit-testable, no fakes needed);
//! `Governor::tick` is thin orchestration over ports (tested with fakes + virtual clock).

use fogar_ports::{
    clock::Clock,
    system_probe::{SystemProbe, SystemSnapshot, Thermal},
    uplink::Uplink,
};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkMode {
    Paused = 0,
    Reduced = 1,
    Full = 2,
}

#[derive(Debug, Clone)]
pub struct Policy {
    pub min_battery_for_full: f32,
    pub max_load_for_full: f32,
    pub poll_active: Duration,
    /// Poll slower when nothing changes / on battery: energy budget.
    pub poll_idle: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            min_battery_for_full: 0.3,
            max_load_for_full: 4.0,
            poll_active: Duration::from_secs(5),
            poll_idle: Duration::from_secs(60),
        }
    }
}

/// Pure decision. Unknown (None) values never block work: degrade gracefully on
/// platforms whose adapter reports `Unsupported`.
pub fn decide(s: &SystemSnapshot, p: &Policy) -> WorkMode {
    if matches!(s.thermal, Some(Thermal::Critical)) {
        return WorkMode::Paused;
    }
    let mut mode = WorkMode::Full;
    if matches!(s.thermal, Some(Thermal::Serious)) {
        mode = mode.min(WorkMode::Reduced);
    }
    if let Some(b) = s.battery
        && !b.on_external_power
        && (b.charge < p.min_battery_for_full || b.low_power_mode == Some(true))
    {
        mode = mode.min(WorkMode::Reduced);
    }
    if s.load_1m.is_some_and(|l| l > p.max_load_for_full) {
        mode = mode.min(WorkMode::Reduced);
    }
    mode
}

#[derive(Debug)]
pub struct Governor<P, U, C> {
    probe: P,
    uplink: U,
    clock: C,
    policy: Policy,
    last: Option<WorkMode>,
    seq: u32,
    buf: Vec<u8>, // reused encode buffer: no per-tick allocation growth over days
}

#[derive(Debug, PartialEq, Eq)]
pub enum TickOutcome {
    Unchanged,
    Reported(WorkMode),
    /// Probe or uplink failed; the governor keeps running (daemon must not die on transient errors).
    Degraded,
}

// Generic (static dispatch): zero boxing in the loop. The composition root names the
// concrete types once. Swap to `Box<dyn SystemProbe>` freely: SystemProbe is dyn-compatible.
impl<P: SystemProbe, U: Uplink, C: Clock> Governor<P, U, C> {
    pub fn new(probe: P, uplink: U, clock: C, policy: Policy) -> Self {
        Self {
            probe,
            uplink,
            clock,
            policy,
            last: None,
            seq: 0,
            buf: Vec::with_capacity(64),
        }
    }

    pub fn mode(&self) -> Option<WorkMode> {
        self.last
    }

    /// One iteration. Returns what happened; the caller (composition root) loops.
    pub async fn tick(&mut self) -> TickOutcome {
        let snap = match self.probe.snapshot() {
            Ok(s) => s,
            Err(_) => return TickOutcome::Degraded, // real code: log + backoff via Clock
        };
        let mode = decide(&snap, &self.policy);
        if self.last == Some(mode) {
            return TickOutcome::Unchanged;
        }
        self.seq = self.seq.wrapping_add(1);
        self.buf.clear();
        fogar_proto::encode_into(
            &fogar_proto::Message::Status {
                seq: self.seq,
                mode: mode as u8,
            },
            &mut self.buf,
        );
        match self.uplink.send(self.buf.clone()).await {
            Ok(()) => {
                self.last = Some(mode);
                TickOutcome::Reported(mode)
            }
            Err(_) => TickOutcome::Degraded,
        }
    }

    /// Next poll interval: slow down when stable (energy).
    pub fn next_interval(&self, outcome: &TickOutcome) -> Duration {
        match outcome {
            TickOutcome::Unchanged => self.policy.poll_idle,
            _ => self.policy.poll_active,
        }
    }

    /// Run forever (until the future is dropped: cancel-safe at every await point,
    /// because state is only mutated after each await completes).
    pub async fn run(&mut self) {
        loop {
            let o = self.tick().await;
            let d = self.next_interval(&o);
            self.clock.sleep(d).await;
        }
    }
}
