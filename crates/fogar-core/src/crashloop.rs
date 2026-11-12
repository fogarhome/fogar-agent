//! Crash-loop breaker (ADR-0003; fogar-longrun-reliability "Panic policy"). Pure decision over a
//! boot ledger; the composition root owns the file (append + fsync) and the delay.
//!
//! Ledger semantics: each boot appends `Start`; a graceful exit appends `Clean`; 10 min of uptime
//! appends `Stable`. A `Start` not followed by `Clean`/`Stable` before the next `Start` is a crash.

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
    pub window: Duration,
    /// Crashes inside `window` that trip SafeMode.
    pub threshold: u32,
    pub first_delay: Duration,
    pub max_delay: Duration,
}

impl Default for BreakerPolicy {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(600),
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
    SafeMode { recent_crashes: u32, delay: Duration },
}

impl BootMode {
    pub fn label(&self) -> &'static str {
        match self {
            BootMode::Normal => "normal",
            BootMode::SafeMode { .. } => "safe",
        }
    }
}

/// Counts crashes in the window ending at `now_ms` and decides the mode for THIS boot (call it
/// before appending this boot's `Start`). Entries may be unsorted; future entries are ignored.
pub fn evaluate(entries: &[LedgerEntry], now_ms: u64, p: &BreakerPolicy) -> BootMode {
    let window_ms = u64::try_from(p.window.as_millis()).unwrap_or(u64::MAX);
    let since = now_ms.saturating_sub(window_ms);
    let mut sorted: Vec<LedgerEntry> =
        entries.iter().copied().filter(|e| e.unix_ms >= since && e.unix_ms <= now_ms).collect();
    sorted.sort_by_key(|e| e.unix_ms);
    let mut crashes = 0u32;
    let mut open_start = false;
    for e in &sorted {
        match e.kind {
            LedgerKind::Start => {
                if open_start {
                    crashes = crashes.saturating_add(1);
                }
                open_start = true;
            }
            LedgerKind::Clean | LedgerKind::Stable => open_start = false,
        }
    }
    // The last boot never wrote Clean/Stable: it crashed too (we are booting again).
    if open_start {
        crashes = crashes.saturating_add(1);
    }
    if crashes < p.threshold {
        return BootMode::Normal;
    }
    let k = crashes - p.threshold;
    let factor = 1u32.checked_shl(k.min(31)).unwrap_or(u32::MAX);
    BootMode::SafeMode { recent_crashes: crashes, delay: p.first_delay.saturating_mul(factor).min(p.max_delay) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn starts(n: u64) -> Vec<LedgerEntry> {
        (0..n).map(|i| LedgerEntry { unix_ms: 1_000 + i * 1_000, kind: LedgerKind::Start }).collect()
    }

    #[test]
    fn trips_after_threshold_and_grows() {
        let p = BreakerPolicy::default();
        assert_eq!(evaluate(&starts(3), 10_000, &p), BootMode::Normal);
        assert_eq!(
            evaluate(&starts(4), 10_000, &p),
            BootMode::SafeMode { recent_crashes: 4, delay: Duration::from_secs(60) }
        );
        assert_eq!(
            evaluate(&starts(5), 10_000, &p),
            BootMode::SafeMode { recent_crashes: 5, delay: Duration::from_secs(120) }
        );
        assert!(matches!(evaluate(&starts(60), 100_000, &p), BootMode::SafeMode { delay, .. } if delay == p.max_delay));
    }

    #[test]
    fn clean_exits_and_old_entries_do_not_count() {
        let p = BreakerPolicy::default();
        let mut e = Vec::new();
        for i in 0..10 {
            e.push(LedgerEntry { unix_ms: i * 10, kind: LedgerKind::Start });
            e.push(LedgerEntry { unix_ms: i * 10 + 5, kind: LedgerKind::Clean });
        }
        assert_eq!(evaluate(&e, 1_000, &p), BootMode::Normal);
        // 5 crashes, but 20 minutes ago.
        assert_eq!(evaluate(&starts(5), 1_000 + 20 * 60_000, &p), BootMode::Normal);
    }
}
