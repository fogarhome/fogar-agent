//! E11: crash-loop breaker. launchd KeepAlive only throttles respawns (default
//! ThrottleInterval 10 s); it never gives up. The process must notice it is
//! crash-looping and degrade itself instead of hammering the gateway/CPU.
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, PartialEq)]
pub enum BootMode { Normal, SafeMode { delay: Duration, recent_crashes: usize } }

const WINDOW_MS: u64 = 10 * 60 * 1000; // 10 min
const THRESHOLD: usize = 4;             // unclean starts in window

fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }

/// Ledger lines: "<unix_ms> start" | "<unix_ms> clean" | "<unix_ms> stable".
/// A "start" not followed by "clean"/"stable" counts as a crash.
pub fn on_boot(ledger: &Path) -> std::io::Result<BootMode> {
    let text = std::fs::read_to_string(ledger).unwrap_or_default();
    let now = now_ms();
    let mut unclean = 0usize;
    let mut pending_start = false;
    let mut kept: Vec<&str> = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(ts), Some(kind)) = (it.next().and_then(|t| t.parse::<u64>().ok()), it.next()) else { continue };
        if now.saturating_sub(ts) > WINDOW_MS { continue; } // also bounds the file size
        kept.push(line);
        match kind {
            "start" => { if pending_start { unclean += 1; } pending_start = true; }
            "clean" | "stable" => pending_start = false,
            _ => {}
        }
    }
    if pending_start { unclean += 1; } // previous run never marked clean
    // rewrite compacted ledger + this start, fsync (cheap: a few lines)
    let mut f = std::fs::File::create(ledger)?;
    for l in &kept { writeln!(f, "{l}")?; }
    writeln!(f, "{now} start")?;
    f.sync_all()?;
    Ok(if unclean >= THRESHOLD {
        let exp = (unclean - THRESHOLD).min(5) as u32;
        BootMode::SafeMode { delay: Duration::from_secs(60 * 2u64.pow(exp)), recent_crashes: unclean }
    } else { BootMode::Normal })
}

pub fn mark(ledger: &Path, kind: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().append(true).create(true).open(ledger)?;
    writeln!(f, "{} {kind}", now_ms())?;
    f.sync_all()
}

fn main() {
    let p = std::env::temp_dir().join(format!("e11-{}.ledger", std::process::id()));
    for i in 0..6 {
        let mode = on_boot(&p).unwrap();
        println!("boot {i}: {mode:?}"); // simulate crash: never mark clean
    }
    mark(&p, "clean").unwrap();
    println!("after clean shutdown: {:?}", on_boot(&p).unwrap());
    let _ = std::fs::remove_file(&p);
}
