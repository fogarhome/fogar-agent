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
        let _ = std::fs::create_dir_all(dir);
    }
    let too_big = std::fs::metadata(&path)
        .map(|m| m.len() > MAX_RECORD_BYTES)
        .unwrap_or(false);
    let mut opts = std::fs::OpenOptions::new();
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
