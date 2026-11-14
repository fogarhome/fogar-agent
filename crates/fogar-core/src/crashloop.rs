//! Crash-loop breaker (ADR-0003; fogar-longrun-reliability "Panic policy"). Pure decision over a
//! boot ledger; the composition root owns the file (append + fsync) and the delay.
//!
//! Ledger semantics: each boot appends `Start`; a graceful exit appends `Clean`; 10 min of uptime
//! appends `Stable`. A `Start` not followed by `Clean`/`Stable` before the next `Start` is a crash.
//!
//! Policy (ADR-0003, aligned in review code#7a): a *run* is the sequence of crashes since the last
//! `Clean`/`Stable`. SafeMode trips when `threshold` crashes of the run fall inside `window`
//! (10 min), and then stays tripped for the rest of the run: every further crash doubles the delay
//! up to `max_delay`.
//!
//! Round-2 review F4: the trip is PERSISTED. A boot that starts in SafeMode records a `Safe`
//! marker next to its `Start`, and a `Safe` marker inside the run keeps it tripped, so the delay
//! never falls back to 0 when the run's first (fast) crashes age out of the ledger. The run ends
//! only with `Clean`/`Stable`, or with a quiet gap longer than `retention` (3 h) between two
//! entries or since the last one (machine off: the next boot starts afresh). A crash loop at the
//! 32-min ceiling never leaves such a gap. `retain` keeps the run (bounded to
//! `MAX_LEDGER_ENTRIES`, recent ones first; the run's newest `Safe` marker is always among them).

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerKind {
    Start,
    Clean,
    Stable,
    /// This boot started in SafeMode (persisted trip; written right after its `Start`).
    Safe,
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
    let window_ms = ms(p.window);
    let threshold = usize::try_from(p.threshold.max(1)).unwrap_or(usize::MAX);
    let sorted = current_run(entries, now_ms, p);
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
            LedgerKind::Safe => tripped = true,
            // `current_run` starts after the last Clean/Stable.
            LedgerKind::Clean | LedgerKind::Stable => {}
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

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The current run, sorted: entries after the last `Clean`/`Stable` and after the last quiet gap
/// longer than `retention` (future entries ignored; empty if the newest entry is older than
/// `retention`).
fn current_run(entries: &[LedgerEntry], now_ms: u64, p: &BreakerPolicy) -> Vec<LedgerEntry> {
    let gap = ms(p.retention);
    let mut sorted: Vec<LedgerEntry> = entries
        .iter()
        .copied()
        .filter(|e| e.unix_ms <= now_ms)
        .collect();
    sorted.sort_by_key(|e| e.unix_ms); // stable: Start before its Safe marker
    if sorted
        .last()
        .is_none_or(|e| now_ms.saturating_sub(e.unix_ms) > gap)
    {
        return Vec::new();
    }
    let mut from = 0;
    for (i, e) in sorted.iter().enumerate() {
        if matches!(e.kind, LedgerKind::Clean | LedgerKind::Stable) {
            from = i + 1;
        } else if i > 0 && e.unix_ms.saturating_sub(sorted[i - 1].unix_ms) > gap {
            from = i;
        }
    }
    sorted.split_off(from)
}

/// Upper bound on the entries kept in the ledger.
pub const MAX_LEDGER_ENTRIES: usize = 64;

/// Appends this boot's entries (call after `evaluate` with its result): `Start`, plus the
/// persisted-trip marker `Safe` when this boot runs in SafeMode.
pub fn record_boot(entries: &mut Vec<LedgerEntry>, now_ms: u64, mode: BootMode) {
    entries.push(LedgerEntry {
        unix_ms: now_ms,
        kind: LedgerKind::Start,
    });
    if matches!(mode, BootMode::SafeMode { .. }) {
        entries.push(LedgerEntry {
            unix_ms: now_ms,
            kind: LedgerKind::Safe,
        });
    }
}

/// The entries the ledger keeps after this boot (the composition root rewrites the file with
/// them): the current run only, at most `MAX_LEDGER_ENTRIES`, newest kept. Dropping the run's
/// oldest entries is safe: once tripped, every boot wrote a `Safe` marker, and the newest ones are
/// kept; and the delay is already at `max_delay` long before 64 entries.
pub fn retain(entries: &[LedgerEntry], now_ms: u64, p: &BreakerPolicy) -> Vec<LedgerEntry> {
    let mut run = current_run(entries, now_ms, p);
    let excess = run.len().saturating_sub(MAX_LEDGER_ENTRIES);
    run.drain(..excess);
    run
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

    /// Round-2 review F4: one boot of a crash-looping daemon, exactly as the composition root does
    /// it (evaluate, record, prune), then a run of `run_ms` before the next crash + 10 s restart.
    fn boot(
        entries: &mut Vec<LedgerEntry>,
        now: &mut u64,
        p: &BreakerPolicy,
        run_ms: u64,
    ) -> Duration {
        let mode = evaluate(entries, *now, p);
        record_boot(entries, *now, mode);
        *entries = retain(entries, *now, p);
        let delay = match mode {
            BootMode::Normal => Duration::ZERO,
            BootMode::SafeMode { delay, .. } => delay,
        };
        *now += u64::try_from(delay.as_millis()).unwrap() + run_ms + 10_000;
        delay
    }

    /// Round-2 review F4: over 40 boots of a sustained crash loop the SafeMode delay never drops
    /// back once tripped (before the fix: 60 s -> 1920 s, then 0 s again after the first entries
    /// aged past `retention`, a sawtooth).
    #[test]
    fn forty_boot_crash_loop_never_drops_back_to_zero() {
        let p = BreakerPolicy::default();
        let (mut entries, mut now) = (Vec::new(), 1_000_000_000u64);
        let delays: Vec<Duration> = (0..40)
            .map(|_| boot(&mut entries, &mut now, &p, 5_000))
            .collect();
        let first_trip = delays.iter().position(|d| !d.is_zero()).unwrap();
        assert_eq!(first_trip, 4, "{delays:?}");
        for w in delays[first_trip..].windows(2) {
            assert!(w[1] >= w[0], "delay dropped: {delays:?}");
        }
        assert_eq!(*delays.last().unwrap(), p.max_delay, "{delays:?}");
        assert!(
            entries.len() <= MAX_LEDGER_ENTRIES,
            "ledger bounded: {}",
            entries.len()
        );
    }

    /// A stable run still resets a tripped breaker, and a long quiet gap (machine off for longer
    /// than `retention`) ends the run too.
    #[test]
    fn tripped_breaker_resets_only_on_stable_or_a_long_gap() {
        let p = BreakerPolicy::default();
        let (mut entries, mut now) = (Vec::new(), 1_000_000_000u64);
        for _ in 0..20 {
            boot(&mut entries, &mut now, &p, 5_000);
        }
        assert!(matches!(
            evaluate(&entries, now, &p),
            BootMode::SafeMode { .. }
        ));
        let mut stable = entries.clone();
        stable.push(LedgerEntry {
            unix_ms: now - 5_000,
            kind: LedgerKind::Stable,
        });
        assert_eq!(evaluate(&stable, now, &p), BootMode::Normal);
        let later = now + u64::try_from(p.retention.as_millis()).unwrap() + 1;
        assert_eq!(evaluate(&entries, later, &p), BootMode::Normal);
    }
}
