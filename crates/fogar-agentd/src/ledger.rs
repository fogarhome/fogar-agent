//! Boot ledger for the crash-loop breaker (ADR-0003). Policy lives in `fogar_core::crashloop`;
//! this file only persists lines `<unix_ms> start|clean|stable`, bounded to the policy window.

use crate::state_dir;
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
        // A symlinked ledger is ignored (and replaced below), never followed (review sec#1).
        let entries: Vec<LedgerEntry> = state_dir::read_private(&self.path)
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
        // The state dir itself was validated by the caller (state_dir::ensure_private_dir).
        if let Err(e) = state_dir::replace_private(&self.path, out.as_bytes()) {
            eprintln!("fogar-agentd: boot ledger not writable ({e}); crash-loop breaker degraded");
        }
        mode
    }

    fn append(&self, kind: LedgerKind) {
        let r = state_dir::private_file()
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

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fogar-ledger-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Review sec#1: symlinks planted at `boot.tmp` / `boot.ledger` are never followed.
    #[cfg(unix)]
    #[test]
    fn planted_symlinks_are_not_followed() {
        let dir = tmpdir("sym");
        let victim_dir = tmpdir("victim");
        let v1 = victim_dir.join("important.conf");
        let v2 = victim_dir.join("other.ledger");
        std::fs::write(&v1, "DO NOT TOUCH\n").unwrap();
        let now = now_ms();
        let planted: String = (0..12)
            .map(|i| format!("{} start\n", now - 1000 - i))
            .collect();
        std::fs::write(&v2, &planted).unwrap();
        std::os::unix::fs::symlink(&v1, dir.join("boot.tmp")).unwrap();
        std::os::unix::fs::symlink(&v2, dir.join("boot.ledger")).unwrap();
        let l = Ledger::new(&dir);
        // A symlinked ledger is not trusted: it cannot trip SafeMode.
        assert_eq!(l.begin_boot(), BootMode::Normal);
        l.mark_clean();
        assert_eq!(std::fs::read_to_string(&v1).unwrap(), "DO NOT TOUCH\n");
        assert_eq!(std::fs::read_to_string(&v2).unwrap(), planted);
        let md = std::fs::symlink_metadata(dir.join("boot.ledger")).unwrap();
        assert!(
            md.file_type().is_file(),
            "ledger replaced by a regular file"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&victim_dir);
    }

    #[cfg(unix)]
    #[test]
    fn append_never_follows_a_symlinked_ledger() {
        let dir = tmpdir("app");
        let victim_dir = tmpdir("appvictim");
        let v = victim_dir.join("authorized_keys");
        std::fs::write(&v, "ssh-ed25519 AAAA\n").unwrap();
        std::os::unix::fs::symlink(&v, dir.join("boot.ledger")).unwrap();
        Ledger::new(&dir).mark_clean();
        assert_eq!(std::fs::read_to_string(&v).unwrap(), "ssh-ed25519 AAAA\n");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&victim_dir);
    }

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
