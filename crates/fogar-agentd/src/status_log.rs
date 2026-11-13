//! Rate limit for the daemon's stderr status lines (review code#1/sec#2): whatever the gateway
//! does, at most `burst` ordinary link-state lines per `window` reach stderr (launchd/journald),
//! plus one summary line counting what was suppressed.
//!
//! Round-2 review F3: terminal/operator-relevant states (Parked, NotPaired, shutting down,
//! stopped) are NEVER suppressed, and the suppressed count is flushed on its own (the caller arms
//! a timer when `check` first suppresses, and flushes at shutdown) instead of waiting for the next
//! printed line, which may never come.

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

    /// `important` lines are always printed (they still use the burst, so ordinary lines after
    /// them stay bounded).
    pub(crate) fn check(&mut self, now: Instant, important: bool) -> Verdict {
        let fresh = self
            .window_start
            .is_none_or(|s| now.saturating_duration_since(s) >= self.window);
        if fresh {
            self.window_start = Some(now);
            self.used = 0;
        }
        if important || self.used < self.burst {
            self.used = self.used.saturating_add(1);
            let s = std::mem::take(&mut self.suppressed);
            Verdict::Print((s > 0).then_some(s))
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            Verdict::Suppress
        }
    }

    /// The number of lines suppressed since the last report, reset to zero (timer / shutdown).
    pub(crate) fn take_suppressed(&mut self) -> Option<u64> {
        let s = std::mem::take(&mut self.suppressed);
        (s > 0).then_some(s)
    }
}

/// Link states an operator must always see on stderr (never rate-limited).
pub(crate) fn always_log(state: &fogar_core::LinkState) -> bool {
    use fogar_core::LinkState as S;
    matches!(
        state,
        S::Parked { .. } | S::NotPaired | S::ShuttingDown | S::Stopped
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_lines_per_window_and_reports_suppressed() {
        let mut l = LogLimiter::new(Duration::from_secs(60), 5);
        let t0 = Instant::now();
        let printed = (0..1000)
            .filter(|i| {
                matches!(
                    l.check(t0 + Duration::from_millis(*i), false),
                    Verdict::Print(_)
                )
            })
            .count();
        assert_eq!(printed, 5);
        assert_eq!(
            l.check(t0 + Duration::from_secs(61), false),
            Verdict::Print(Some(995))
        );
        assert_eq!(
            l.check(t0 + Duration::from_secs(62), false),
            Verdict::Print(None)
        );
    }

    /// Round-2 review F3: after the burst is used up, a terminal state still prints, with the
    /// suppressed count in front of it.
    #[test]
    fn important_lines_are_never_suppressed() {
        let mut l = LogLimiter::new(Duration::from_secs(60), 3);
        let t0 = Instant::now();
        for _ in 0..10 {
            l.check(t0, false);
        }
        assert_eq!(l.check(t0, false), Verdict::Suppress);
        assert_eq!(l.check(t0, true), Verdict::Print(Some(8)));
        assert_eq!(l.check(t0, true), Verdict::Print(None));
        assert_eq!(l.check(t0, false), Verdict::Suppress);
    }

    #[test]
    fn suppressed_count_can_be_flushed_without_a_new_line() {
        let mut l = LogLimiter::new(Duration::from_secs(60), 1);
        let t0 = Instant::now();
        assert_eq!(l.take_suppressed(), None);
        for _ in 0..5 {
            l.check(t0, false);
        }
        assert_eq!(l.take_suppressed(), Some(4));
        assert_eq!(l.take_suppressed(), None);
    }

    #[test]
    fn terminal_states_are_always_logged() {
        use fogar_core::LinkState as S;
        assert!(always_log(&S::NotPaired));
        assert!(always_log(&S::ShuttingDown));
        assert!(always_log(&S::Stopped));
        assert!(!always_log(&S::Ready { session: 1 }));
        assert!(!always_log(&S::Connecting { attempt: 1 }));
    }
}
