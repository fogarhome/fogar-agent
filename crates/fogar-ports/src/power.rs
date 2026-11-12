//! PowerPolicy port: execution priority / QoS intent (fogar-energy-budget "ExecutionPolicy").
//!
//! The core and the composition root state INTENT (`WorkClass`); adapters map it to the OS:
//! macOS `pthread_set_qos_class_self_np` (UTILITY for control, BACKGROUND for bulk),
//! Windows EcoQoS (`SetThreadInformation` power throttling), Linux `SCHED_IDLE`/nice/timer slack.
//! Never `setpriority`/`pthread_setschedparam` on macOS QoS threads (opts out of QoS).
//!
//! Sync and dyn-compatible: called once per thread start. Unsupported is data, not an error.

use crate::Support;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WorkClass {
    /// Runtime workers, network, protocol: energy-efficient but responsive.
    Control,
    /// Deferrable batch work (never the socket-owning thread).
    Bulk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PowerCaps {
    pub thread_class: Support,
    pub process_baseline: Support,
}

impl PowerCaps {
    pub const NONE: Self = Self {
        thread_class: Support::Unsupported,
        process_baseline: Support::Unsupported,
    };
}

/// What actually happened, for health reports and the energy gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Applied,
    /// The capability is Unsupported on this platform: nothing was changed.
    NotSupported,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PowerError {
    #[error("platform refused the policy")]
    Platform(#[source] Box<dyn std::error::Error + Send + Sync>),
}

pub trait PowerPolicy: Send + Sync + 'static {
    /// Stable for the adapter's lifetime.
    fn capabilities(&self) -> PowerCaps;
    /// Process-wide defaults (e.g. timer slack, background I/O). Call once at start.
    fn apply_process_baseline(&self) -> Result<Applied, PowerError>;
    /// Applies `class` to the CALLING thread. Cheap (< 50 µs); safe to call from `on_thread_start`.
    fn apply_current_thread(&self, class: WorkClass) -> Result<Applied, PowerError>;
}

const _: Option<&dyn PowerPolicy> = None;
