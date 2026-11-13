use fogar_ports::clock::{Clock, Mono, SuspendBehavior};
use std::time::Duration;
use tokio::time::Instant;

/// Clock over tokio time: real time normally, virtual time under `start_paused = true`
/// (auto-advance when every task is idle), so days of timers run in milliseconds.
#[derive(Debug, Clone, Copy)]
pub struct TestClock {
    epoch: Instant,
}

impl TestClock {
    /// Must be created inside a runtime when time is paused.
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for TestClock {
    fn suspend_behavior(&self) -> SuspendBehavior {
        SuspendBehavior::Virtual
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
