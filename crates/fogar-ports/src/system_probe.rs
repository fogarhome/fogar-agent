//! SystemProbe port: load / memory / thermal / battery / idle.
//!
//! Design choices (see references/port-design.md):
//! - SYNC methods: reading these values is a cheap syscall on every target OS;
//!   making them async would force boxing for `dyn` and buys nothing.
//! - Capability query up front (`capabilities`) + `Option` fields: "unsupported" is data, not an error.
//! - Push changes via a bounded `EventStream` when the OS can notify (macOS thermal
//!   state, power source changes); otherwise `subscribe` returns `Unsupported` and the
//!   core polls `snapshot` at a cadence it chooses.
//! - `dyn`-compatible on purpose: the composition root may pick an adapter at runtime.

use crate::{Support, events::EventStream};
use std::{num::NonZeroUsize, time::Duration};

/// Declared once per adapter instance; must not change during its lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProbeCaps {
    pub load: Support,
    pub memory: Support,
    pub thermal: Support,
    pub battery: Support,
    pub idle: Support,
    pub change_events: Support,
}

impl ProbeCaps {
    /// Nothing supported - the honest default for a new adapter.
    pub const NONE: ProbeCaps = ProbeCaps {
        load: Support::Unsupported,
        memory: Support::Unsupported,
        thermal: Support::Unsupported,
        battery: Support::Unsupported,
        idle: Support::Unsupported,
        change_events: Support::Unsupported,
    };
}

/// Mirrors macOS `ProcessInfo.ThermalState` (macOS-first, without hiding its strength);
/// other OSes map onto it or report `Unsupported`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Thermal {
    Nominal,
    Fair,
    Serious,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Memory {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Battery {
    /// 0.0 ..= 1.0
    pub charge: f32,
    pub on_external_power: bool,
    /// OS "low power mode" / "battery saver" when the OS exposes it.
    pub low_power_mode: Option<bool>,
}

/// One consistent reading. Field is `None` iff the matching capability is `Unsupported`
/// (or the value is momentarily unknown, e.g. no battery present).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[non_exhaustive]
pub struct SystemSnapshot {
    pub load_1m: Option<f32>,
    pub memory: Option<Memory>,
    pub thermal: Option<Thermal>,
    pub battery: Option<Battery>,
    pub idle_for: Option<Duration>,
}

impl SystemSnapshot {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_load(mut self, v: f32) -> Self {
        self.load_1m = Some(v);
        self
    }
    pub fn with_memory(mut self, m: Memory) -> Self {
        self.memory = Some(m);
        self
    }
    pub fn with_thermal(mut self, t: Thermal) -> Self {
        self.thermal = Some(t);
        self
    }
    pub fn with_battery(mut self, b: Battery) -> Self {
        self.battery = Some(b);
        self
    }
    pub fn with_idle(mut self, d: Duration) -> Self {
        self.idle_for = Some(d);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum ProbeEvent {
    ThermalChanged(Thermal),
    PowerSourceChanged {
        on_external_power: bool,
    },
    LowPowerModeChanged(bool),
    /// System is about to sleep / just woke: core should checkpoint / resync timers.
    WillSleep,
    DidWake,
}

/// Port-level error. Adapters map OS errors into these; the platform detail is kept
/// as a boxed source for logs, never matched on by the core.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProbeError {
    #[error("not supported by this adapter: {0}")]
    Unsupported(&'static str),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// Worth retrying later (resource busy, OS service restarting...).
    #[error("transient failure")]
    Transient(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("platform failure")]
    Platform(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl ProbeError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, ProbeError::Transient(_))
    }
}

pub trait SystemProbe: Send + Sync + 'static {
    /// Stable for the adapter's lifetime. Cheap.
    fn capabilities(&self) -> ProbeCaps;

    /// Must be cheap (target: < 1 ms, no allocation per call is a bonus) because the
    /// core may call it every few seconds for days. Must not block on I/O that can hang.
    fn snapshot(&self) -> Result<SystemSnapshot, ProbeError>;

    /// Subscribe to OS-pushed changes. Default: unsupported (graceful degradation;
    /// adding this method later with a default would be a non-breaking change).
    fn subscribe(&self, capacity: NonZeroUsize) -> Result<EventStream<ProbeEvent>, ProbeError> {
        let _ = capacity;
        Err(ProbeError::Unsupported("change_events"))
    }
}

// Compile-time guard: the port must stay dyn-compatible (same idea as notify's
// `test_object_safe`). Adding an `async fn` or a generic method breaks this line.
const _: Option<&dyn SystemProbe> = None;
