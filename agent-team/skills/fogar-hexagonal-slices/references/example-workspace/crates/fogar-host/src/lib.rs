//! Portable adapters (std + tokio), reused by fogar-agent-macos / -windows / -linux.
#![forbid(unsafe_code)]

use fogar_ports::{
    clock::{Clock, Mono},
    uplink::{Uplink, UplinkError},
};
use std::time::Duration;
// tokio's Instant (not std's): honours paused/auto-advanced time in tests.
use tokio::time::Instant;

#[derive(Debug, Clone)]
pub struct TokioClock {
    epoch: Instant,
}

impl Default for TokioClock {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
}

impl Clock for TokioClock {
    fn now(&self) -> Mono {
        Mono(self.epoch.elapsed())
    }
    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await // cancel-safe: dropping the Sleep cancels the timer
    }
}

/// Walking-skeleton uplink: logs frames to stderr. Replaced by the real gateway
/// transport in a later slice WITHOUT touching core (same port).
#[derive(Debug, Default, Clone)]
pub struct StderrUplink;

impl Uplink for StderrUplink {
    fn max_frame(&self) -> usize {
        64 * 1024
    }
    async fn send(&self, frame: Vec<u8>) -> Result<(), UplinkError> {
        if frame.len() > Uplink::max_frame(self) {
            return Err(UplinkError::TooLarge {
                len: frame.len(),
                max: Uplink::max_frame(self),
            });
        }
        eprintln!("uplink frame: {frame:02x?}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn stderr_uplink_passes_uplink_contract() {
        fogar_testkit::contract::uplink::rejects_oversized_async(&StderrUplink).await;
        fogar_testkit::contract::uplink::survives_cancelled_send_async(&StderrUplink).await;
    }

    #[tokio::test(start_paused = true)]
    async fn tokio_clock_sleep_advances() {
        let c = TokioClock::default();
        let a = c.now();
        c.sleep(Duration::from_secs(3600)).await; // paused runtime: instant
        assert!(c.now().0 - a.0 >= Duration::from_secs(3600));
    }
}
