//! Crash-loop breaker (ADR-0003; fogar-longrun-reliability "Panic policy"). Pure decision over a
//! boot ledger; the composition root owns the file (append + fsync) and the delay.
//!
//! Ledger semantics: each boot appends `Start`; a graceful exit appends `Clean`; 10 min of uptime
//! appends `Stable`. A `Start` not followed by `Clean`/`Stable` before the next `Start` is a crash.
//!
//! Policy (ADR-0003, aligned in review code#7a): a *run* is the sequence of crashes since the last
//! `Clean`/`Stable`. SafeMode trips when `threshold` crashes of the run fall inside `window`
//! (10 min), and then stays tripped for the rest of the run: every further crash doubles the delay
//! up to `max_delay`. The ledger keeps `retention` (3 h) so the run is still visible while the
//! delays themselves stretch it far beyond 10 min (60+120+...+1920 s = 62 min).

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerKind {
    Start,
    Clean,
    Stable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerEntry {
    pub unix_ms: u64,
    pub kind: LedgerKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakerPolicy {
    /// Trip window: `threshold` crashes inside it start SafeMode.
    pub window: Duration,
    /// How long ledger entries are kept and considered (must cover a whole escalation).
    pub retention: Duration,
    /// Crashes inside `window` that trip SafeMode.
    pub threshold: u32,
    pub first_delay: Duration,
    pub max_delay: Duration,
}

impl Default for BreakerPolicy {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(600),
            retention: Duration::from_secs(3 * 3600),
            threshold: 4,
            first_delay: Duration::from_secs(60),
            max_delay: Duration::from_secs(32 * 60),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootMode {
    Normal,
    /// Wait `delay` (cancellable by SIGTERM), then run with optional slices disabled.
    SafeMode {
        recent_crashes: u32,
        delay: Duration,
    },
}

impl BootMode {
    pub fn label(&self) -> &'static str {
        match self {
            BootMode::Normal => "normal",
            BootMode::SafeMode { .. } => "safe",
        }
    }
}

/// Decides the mode for THIS boot from the ledger (call it before appending this boot's
/// `Start`). Entries may be unsorted; future entries and entries older than `retention` are
/// ignored.
pub fn evaluate(entries: &[LedgerEntry], now_ms: u64, p: &BreakerPolicy) -> BootMode {
    let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    let since = now_ms.saturating_sub(ms(p.retention));
    let window_ms = ms(p.window);
    let threshold = usize::try_from(p.threshold.max(1)).unwrap_or(usize::MAX);
    let mut sorted: Vec<LedgerEntry> = entries
        .iter()
        .copied()
        .filter(|e| e.unix_ms >= since && e.unix_ms <= now_ms)
        .collect();
    sorted.sort_by_key(|e| e.unix_ms);
    // Start times of the crashed boots of the current run.
    let mut run: Vec<u64> = Vec::new();
    let mut tripped = false;
    let mut open_start: Option<u64> = None;
    let crashed = |t: u64, run: &mut Vec<u64>, tripped: &mut bool| {
        run.push(t);
        if run.len() >= threshold {
            let first = run[run.len() - threshold];
            if t.saturating_sub(first) <= window_ms {
                *tripped = true;
            }
        }
    };
    for e in &sorted {
        match e.kind {
            LedgerKind::Start => {
                if let Some(t) = open_start {
                    crashed(t, &mut run, &mut tripped);
                }
                open_start = Some(e.unix_ms);
            }
            LedgerKind::Clean | LedgerKind::Stable => {
                open_start = None;
                run.clear();
                tripped = false;
            }
        }
    }
    // The last boot never wrote Clean/Stable: it crashed too (we are booting again).
    if let Some(t) = open_start {
        crashed(t, &mut run, &mut tripped);
    }
    if !tripped {
        return BootMode::Normal;
    }
    let crashes = u32::try_from(run.len()).unwrap_or(u32::MAX);
    let k = crashes.saturating_sub(p.threshold);
    let factor = 1u32.checked_shl(k.min(31)).unwrap_or(u32::MAX);
    BootMode::SafeMode {
        recent_crashes: crashes,
        delay: p.first_delay.saturating_mul(factor).min(p.max_delay),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn starts(n: u64) -> Vec<LedgerEntry> {
        (0..n)
            .map(|i| LedgerEntry {
                unix_ms: 1_000 + i * 1_000,
                kind: LedgerKind::Start,
            })
            .collect()
    }

    #[test]
    fn trips_after_threshold_and_grows() {
        let p = BreakerPolicy::default();
        assert_eq!(evaluate(&starts(3), 10_000, &p), BootMode::Normal);
        assert_eq!(
            evaluate(&starts(4), 10_000, &p),
            BootMode::SafeMode {
                recent_crashes: 4,
                delay: Duration::from_secs(60)
            }
        );
        assert_eq!(
            evaluate(&starts(5), 10_000, &p),
            BootMode::SafeMode {
                recent_crashes: 5,
                delay: Duration::from_secs(120)
            }
        );
        assert!(
            matches!(evaluate(&starts(60), 100_000, &p), BootMode::SafeMode { delay, .. } if delay == p.max_delay)
        );
    }

    /// Review code#7a: a real crash loop (each boot waits its SafeMode delay, runs ~5 s, crashes,
    /// is restarted ~10 s later) must escalate all the way to `max_delay` (ADR-0003: 32 min).
    #[test]
    fn a_sustained_crash_loop_reaches_the_maximum_delay() {
        let p = BreakerPolicy::default();
        let mut entries = Vec::new();
        let mut now: u64 = 1_000_000_000;
        let mut max_seen = Duration::ZERO;
        for _ in 0..12 {
            let delay = match evaluate(&entries, now, &p) {
                BootMode::Normal => Duration::ZERO,
                BootMode::SafeMode { delay, .. } => delay,
            };
            max_seen = max_seen.max(delay);
            entries.push(LedgerEntry {
                unix_ms: now,
                kind: LedgerKind::Start,
            });
            now += u64::try_from(delay.as_millis()).unwrap() + 15_000;
        }
        assert_eq!(max_seen, p.max_delay);
    }

    /// A clean or stable boot ends the escalation.
    #[test]
    fn a_stable_boot_resets_the_escalation() {
        let p = BreakerPolicy::default();
        let mut e = starts(8);
        e.push(LedgerEntry {
            unix_ms: 9_500,
            kind: LedgerKind::Stable,
        });
        e.push(LedgerEntry {
            unix_ms: 9_600,
            kind: LedgerKind::Start,
        });
        assert_eq!(evaluate(&e, 10_000, &p), BootMode::Normal);
    }

    #[test]
    fn clean_exits_and_old_entries_do_not_count() {
        let p = BreakerPolicy::default();
        let mut e = Vec::new();
        for i in 0..10 {
            e.push(LedgerEntry {
                unix_ms: i * 10,
                kind: LedgerKind::Start,
            });
            e.push(LedgerEntry {
                unix_ms: i * 10 + 5,
                kind: LedgerKind::Clean,
            });
        }
        assert_eq!(evaluate(&e, 1_000, &p), BootMode::Normal);
        // 5 crashes, but older than the retention (4 h ago).
        assert_eq!(
            evaluate(&starts(5), 1_000 + 4 * 3_600_000, &p),
            BootMode::Normal
        );
        // 4 crashes spread over 40 minutes never trip (not inside one 10-minute window).
        let spread: Vec<_> = (0..4)
            .map(|i| LedgerEntry {
                unix_ms: 1_000 + i * 13 * 60_000,
                kind: LedgerKind::Start,
            })
            .collect();
        assert_eq!(evaluate(&spread, 1_000 + 40 * 60_000, &p), BootMode::Normal);
    }
}
