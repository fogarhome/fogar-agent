//! Clock port: monotonic time + sleeping. The core never reads ambient time (clippy.toml), so it
//! can be tested in paused/virtual time and so the macOS adapter can add timer leeway later.
//!
//! Contract (see `fogar_testkit::contract::clock`):
//! - `now()` never goes backwards.
//! - `sleep(d)` completes no earlier than `now() + d`; `sleep_until(t)` with `t <= now()` completes
//!   immediately (one poll).
//! - Both futures are **cancel-safe**: dropping them cancels the timer; no state is lost.
//!
//! Suspend semantics are DECLARED by each adapter (`suspend_behavior`), never assumed: std/tokio
//! `Instant` is CLOCK_MONOTONIC on Linux and CLOCK_UPTIME_RAW on macOS, and neither advances while
//! the machine sleeps. Slice 0 does not rely on either answer; S1 adds a wall-vs-monotonic jump
//! detector (fogar-longrun-reliability rule 6) and reconnects after a detected sleep (ADR-0006).

use std::{future::Future, time::Duration};

/// Monotonic instant as a duration since an adapter-defined epoch. A plain value type so fakes
/// can construct it and so it can cross threads cheaply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Mono(pub Duration);

impl Mono {
    pub const ZERO: Mono = Mono(Duration::ZERO);

    pub fn saturating_since(self, earlier: Mono) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
    pub fn saturating_add(self, d: Duration) -> Mono {
        Mono(self.0.saturating_add(d))
    }
}

/// Whether `Clock::now()` keeps counting while the system is suspended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SuspendBehavior {
    /// Stops during suspend (CLOCK_UPTIME_RAW on macOS, CLOCK_MONOTONIC on Linux, tokio time).
    ExcludesSuspend,
    /// Keeps counting through suspend (CLOCK_BOOTTIME, mach_continuous_time, QPC on Windows).
    IncludesSuspend,
    /// Virtual time in tests: suspend does not exist.
    Virtual,
}

pub trait Clock: Send + Sync + 'static {
    /// Declared once; stable for the adapter's lifetime.
    fn suspend_behavior(&self) -> SuspendBehavior;
    fn now(&self) -> Mono;
    /// Sleep at least `d`. Cancel-safe.
    fn sleep(&self, d: Duration) -> impl Future<Output = ()> + Send;
    /// Sleep until `deadline` (on this clock's timeline). Cancel-safe.
    fn sleep_until(&self, deadline: Mono) -> impl Future<Output = ()> + Send;
}
