//! Review sec#1: the state directory is validated (no predictable /tmp fallback, owner, 0700,
//! not a symlink) before anything is read from or written to it.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

const BIN: &str = env!("CARGO_BIN_EXE_fogar-agentd");

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fogar-sd-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn store_key(dir: Option<&Path>, env_clear: bool) -> Output {
    let mut c = Command::new(BIN);
    c.arg("store-key");
    if let Some(d) = dir {
        c.arg("--state-dir").arg(d);
    }
    if env_clear {
        c.env_remove("HOME").env_remove("XDG_STATE_HOME");
    }
    let mut child = c
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The daemon may refuse (and exit) before reading stdin: a broken pipe is fine.
    let _ = child.stdin.take().unwrap().write_all(b"k");
    child.wait_with_output().unwrap()
}

#[test]
fn no_home_means_no_predictable_tmp_fallback() {
    let out = store_key(None, true);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "store-key succeeded without HOME: {err}"
    );
    assert!(err.contains("state directory"), "{err}");
}

#[test]
fn foreign_owned_state_dir_is_refused() {
    if std::fs::metadata("/proc/self").unwrap().uid() != 0 {
        eprintln!("skipped: needs root to plant a directory owned by another user");
        return;
    }
    let dir = tmp("foreign");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    // Owned by `nobody`: what a squatter of a shared path would leave behind.
    assert!(
        Command::new("chown")
            .args(["65534:65534"])
            .arg(&dir)
            .status()
            .unwrap()
            .success()
    );
    let out = store_key(Some(&dir), false);
    assert!(!out.status.success(), "accepted a foreign-owned state dir");
    let out = Command::new(BIN)
        .args([
            "run",
            "--no-ipc",
            "--gateway",
            "ws://127.0.0.1:9/ws",
            "--state-dir",
        ])
        .arg(&dir)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        !dir.join("boot.ledger").exists(),
        "wrote into a foreign-owned dir"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn symlinked_state_dir_is_refused() {
    let base = tmp("symdir");
    let real = base.join("real");
    std::fs::create_dir_all(&real).unwrap();
    let link = base.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let out = store_key(Some(&link), false);
    assert!(!out.status.success(), "accepted a symlinked state dir");
    assert!(!real.join("secrets").exists());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn loose_state_dir_mode_is_tightened() {
    let dir = tmp("loose");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let out = store_key(Some(&dir), false);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review sec#7: the running daemon cannot dump core (the dump would hold the client key).
#[test]
fn daemon_runs_with_core_dumps_disabled() {
    let dir = tmp("core");
    let mut child = Command::new(BIN)
        .args([
            "run",
            "--no-ipc",
            "--gateway",
            "ws://127.0.0.1:9/ws",
            "--state-dir",
        ])
        .arg(&dir)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let limits = format!("/proc/{}/limits", child.id());
    let t0 = std::time::Instant::now();
    let mut line = String::new();
    while t0.elapsed() < std::time::Duration::from_secs(10) {
        std::thread::sleep(std::time::Duration::from_millis(50));
        line = std::fs::read_to_string(&limits)
            .unwrap_or_default()
            .lines()
            .find(|l| l.starts_with("Max core file size"))
            .unwrap_or_default()
            .to_owned();
        if dir.join("boot.ledger").exists() {
            break;
        }
    }
    let _ = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    let _ = child.wait();
    let fields: Vec<_> = line.split_whitespace().collect();
    assert_eq!(fields.get(4..6), Some(&["0", "0"][..]), "{line}");
    let _ = std::fs::remove_dir_all(&dir);
}
