//! Reconnect backoff: capped exponential with FULL jitter (RFC 6455 §7.2.3: the first retry is
//! random too). Pure; randomness comes from the `Random` port.

use fogar_ports::random::{Random, below_inclusive};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackoffPolicy {
    /// Ceiling of the first retry window.
    pub base: Duration,
    /// Ceiling of every retry window.
    pub cap: Duration,
    /// Floor so a burst of failures can never hot-loop (NET-4).
    pub floor: Duration,
    /// A session must stay up this long before the attempt counter resets (anti-flap).
    pub stable_after: Duration,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            base: Duration::from_millis(500),
            cap: Duration::from_secs(60),
            floor: Duration::from_millis(100),
            stable_after: Duration::from_secs(30),
        }
    }
}

impl BackoffPolicy {
    /// Upper bound of the window for `attempt` (0-based): `min(cap, base * 2^attempt)`. Never
    /// overflows, whatever the attempt count.
    pub fn window(&self, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX);
        self.base.saturating_mul(factor).min(self.cap).max(self.floor)
    }

    /// Full jitter: uniform in `[floor, window(attempt)]`.
    pub fn delay(&self, attempt: u32, rng: &dyn Random) -> Duration {
        let hi = self.window(attempt);
        let span = hi.saturating_sub(self.floor);
        let span_ms = u64::try_from(span.as_millis()).unwrap_or(u64::MAX);
        self.floor + Duration::from_millis(below_inclusive(rng, span_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Seq(std::sync::atomic::AtomicU64);
    impl Random for Seq {
        fn next_u64(&self) -> u64 {
            let x = self.0.fetch_add(0x9E37_79B9_7F4A_7C15, std::sync::atomic::Ordering::Relaxed);
            x ^ (x >> 31)
        }
    }

    #[test]
    fn bounds_hold_for_any_attempt() {
        let p = BackoffPolicy::default();
        let r = Seq(std::sync::atomic::AtomicU64::new(7));
        for attempt in [0, 1, 2, 5, 10, 31, 32, 1000, u32::MAX] {
            for _ in 0..200 {
                let d = p.delay(attempt, &r);
                assert!(d >= p.floor && d <= p.cap, "attempt {attempt}: {d:?}");
                assert!(d <= p.window(attempt));
            }
        }
        assert_eq!(p.window(0), Duration::from_millis(500));
        assert_eq!(p.window(3), Duration::from_secs(4));
        assert_eq!(p.window(40), p.cap);
    }

    #[test]
    fn jitter_actually_spreads() {
        let p = BackoffPolicy::default();
        let r = Seq(std::sync::atomic::AtomicU64::new(1));
        let v: std::collections::BTreeSet<_> = (0..50).map(|_| p.delay(6, &r)).collect();
        assert!(v.len() > 40, "full jitter must not collapse to a few values");
    }
}
