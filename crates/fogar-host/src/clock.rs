use fogar_ports::clock::{Clock, Mono, SuspendBehavior};
use std::time::Duration;
use tokio::time::Instant;

/// Production clock over tokio time (so paused-time tests and production share one code path).
///
/// tokio's `Instant` is std's: CLOCK_MONOTONIC on Linux, CLOCK_UPTIME_RAW on macOS. Neither
/// counts system sleep, hence `ExcludesSuspend` (fogar-longrun-reliability rule 6).
#[derive(Debug, Clone, Copy)]
pub struct TokioClock {
    epoch: Instant,
}

impl TokioClock {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
    /// The epoch as a std `Instant`, for the OS-thread watchdog that reads the heartbeat on the
    /// same timeline.
    pub fn epoch_std(&self) -> std::time::Instant {
        self.epoch.into_std()
    }
}

impl Default for TokioClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for TokioClock {
    fn suspend_behavior(&self) -> SuspendBehavior {
        SuspendBehavior::ExcludesSuspend
    }
    fn now(&self) -> Mono {
        Mono(Instant::now().saturating_duration_since(self.epoch))
    }
    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
    async fn sleep_until(&self, deadline: Mono) {
        tokio::time::sleep_until(self.epoch + deadline.0).await;
    }
}
