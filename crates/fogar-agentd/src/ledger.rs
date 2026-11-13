//! Boot ledger for the crash-loop breaker (ADR-0003). Policy lives in `fogar_core::crashloop`;
//! this file only persists lines `<unix_ms> start|clean|stable`, bounded to the policy window.

use fogar_core::crashloop::{BootMode, BreakerPolicy, LedgerEntry, LedgerKind, evaluate};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone)]
pub(crate) struct Ledger {
    path: PathBuf,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn parse(line: &str) -> Option<LedgerEntry> {
    let (ms, kind) = line.split_once(' ')?;
    let kind = match kind.trim() {
        "start" => LedgerKind::Start,
        "clean" => LedgerKind::Clean,
        "stable" => LedgerKind::Stable,
        _ => return None,
    };
    Some(LedgerEntry {
        unix_ms: ms.parse().ok()?,
        kind,
    })
}

fn render(e: &LedgerEntry) -> String {
    let k = match e.kind {
        LedgerKind::Start => "start",
        LedgerKind::Clean => "clean",
        LedgerKind::Stable => "stable",
    };
    format!("{} {k}\n", e.unix_ms)
}

impl Ledger {
    pub(crate) fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("boot.ledger"),
        }
    }

    /// Decides this boot's mode, then records `start` (rewriting the file bounded to the window).
    pub(crate) fn begin_boot(&self) -> BootMode {
        let policy = BreakerPolicy::default();
        let now = now_ms();
        let entries: Vec<LedgerEntry> = std::fs::read_to_string(&self.path)
            .map(|s| s.lines().filter_map(parse).collect())
            .unwrap_or_default();
        let mode = evaluate(&entries, now, &policy);
        let since =
            now.saturating_sub(u64::try_from(policy.window.as_millis()).unwrap_or(u64::MAX));
        let mut out: String = entries
            .iter()
            .filter(|e| e.unix_ms >= since)
            .map(render)
            .collect();
        out.push_str(&render(&LedgerEntry {
            unix_ms: now,
            kind: LedgerKind::Start,
        }));
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.path.with_extension("tmp");
        let ok = std::fs::File::create(&tmp)
            .and_then(|mut f| {
                f.write_all(out.as_bytes())?;
                f.sync_all()
            })
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(e) = ok {
            eprintln!("fogar-agentd: boot ledger not writable ({e}); crash-loop breaker degraded");
        }
        mode
    }

    fn append(&self, kind: LedgerKind) {
        let r = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.path)
            .and_then(|mut f| {
                f.write_all(
                    render(&LedgerEntry {
                        unix_ms: now_ms(),
                        kind,
                    })
                    .as_bytes(),
                )?;
                f.sync_all()
            });
        if let Err(e) = r {
            eprintln!("fogar-agentd: boot ledger append failed: {e}");
        }
    }

    pub(crate) fn mark_clean(&self) {
        self.append(LedgerKind::Clean);
    }

    pub(crate) fn mark_stable(&self) {
        self.append(LedgerKind::Stable);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_render_roundtrip_and_garbage() {
        let e = LedgerEntry {
            unix_ms: 42,
            kind: LedgerKind::Clean,
        };
        assert_eq!(parse(render(&e).trim_end()), Some(e));
        assert_eq!(parse("x start"), None);
        assert_eq!(parse("1 crash"), None);
        assert_eq!(parse(""), None);
    }
}
