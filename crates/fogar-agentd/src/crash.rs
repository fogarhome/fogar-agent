//! Synchronous crash record (ADR-0003): with `panic = "abort"` the non-blocking logger dies with
//! the process, so the hook writes and fsyncs the record itself. Content-free by design: panic
//! messages and locations only (no user content, no secrets; LOG-1).

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Instant,
};

const MAX_RECORD_BYTES: u64 = 1 << 20;
static STARTED: OnceLock<Instant> = OnceLock::new();

fn record_path(state_dir: &Path) -> PathBuf {
    state_dir.join("crash").join("last.txt")
}

/// Appends one record (truncating the file first if it exceeds 1 MiB) and fsyncs it.
pub(crate) fn write_record(state_dir: &Path, what: &str) {
    let path = record_path(state_dir);
    if let Some(dir) = path.parent() {
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir);
        // Never write through a symlinked crash dir (review sec#1).
        if std::fs::symlink_metadata(dir).map_or(true, |m| !m.is_dir()) {
            return;
        }
    }
    let too_big = std::fs::symlink_metadata(&path)
        .map(|m| m.len() > MAX_RECORD_BYTES)
        .unwrap_or(false);
    // O_NOFOLLOW + 0600: a planted `last.txt` symlink makes the open fail instead of clobbering
    // its target.
    let mut opts = crate::state_dir::private_file();
    opts.create(true);
    if too_big {
        opts.write(true).truncate(true);
    } else {
        opts.append(true);
    }
    if let Ok(mut f) = opts.open(&path) {
        let uptime = STARTED.get().map(|s| s.elapsed()).unwrap_or_default();
        let thread = std::thread::current();
        let _ = writeln!(
            f,
            "--- crash uptime={uptime:?} thread={:?}\n{what}",
            thread.name()
        );
        let _ = f.sync_all();
    }
}

pub(crate) fn install_hook(state_dir: &Path) {
    let _ = STARTED.set(Instant::now());
    let dir = state_dir.to_path_buf();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        write_record(&dir, &format!("{info}\n{bt}"));
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review sec#1: a symlink planted at `crash/last.txt` is never followed.
    #[cfg(unix)]
    #[test]
    fn planted_crash_record_symlink_is_not_followed() {
        let base = std::env::temp_dir().join(format!("fogar-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("state");
        std::fs::create_dir_all(dir.join("crash")).unwrap();
        let victim = base.join("victim.txt");
        std::fs::write(&victim, "keep\n").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("crash/last.txt")).unwrap();
        write_record(&dir, "boom");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep\n");
        let _ = std::fs::remove_dir_all(&base);
    }
}
