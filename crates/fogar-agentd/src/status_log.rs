//! Rate limit for the daemon's stderr status lines (review code#1/sec#2): whatever the gateway
//! does, at most `burst` link-state lines per `window` reach stderr (launchd/journald), plus one
//! summary line counting what was suppressed.

use std::time::{Duration, Instant};

#[derive(Debug)]
pub(crate) struct LogLimiter {
    window: Duration,
    burst: u32,
    window_start: Option<Instant>,
    used: u32,
    suppressed: u64,
}

/// What the caller should print for one event.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Print the line; if `Some(n)`, first print "n lines suppressed".
    Print(Option<u64>),
    Suppress,
}

impl LogLimiter {
    pub(crate) fn new(window: Duration, burst: u32) -> Self {
        Self {
            window,
            burst,
            window_start: None,
            used: 0,
            suppressed: 0,
        }
    }

    pub(crate) fn check(&mut self, now: Instant) -> Verdict {
        let fresh = self
            .window_start
            .is_none_or(|s| now.saturating_duration_since(s) >= self.window);
        if fresh {
            self.window_start = Some(now);
            self.used = 0;
        }
        if self.used < self.burst {
            self.used += 1;
            let s = std::mem::take(&mut self.suppressed);
            Verdict::Print((s > 0).then_some(s))
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            Verdict::Suppress
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_lines_per_window_and_reports_suppressed() {
        let mut l = LogLimiter::new(Duration::from_secs(60), 5);
        let t0 = Instant::now();
        let printed = (0..1000)
            .filter(|i| matches!(l.check(t0 + Duration::from_millis(*i)), Verdict::Print(_)))
            .count();
        assert_eq!(printed, 5);
        assert_eq!(
            l.check(t0 + Duration::from_secs(61)),
            Verdict::Print(Some(995))
        );
        assert_eq!(l.check(t0 + Duration::from_secs(62)), Verdict::Print(None));
    }
}
