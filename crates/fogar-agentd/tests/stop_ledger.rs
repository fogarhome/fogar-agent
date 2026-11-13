//! Review code#2 / code#7b: an ordinary stop (SIGTERM) must always leave a `clean` line in the
//! boot ledger, even when the gateway never answers the close handshake and even when the signal
//! arrives right after boot; otherwise four ordinary stops trip SafeMode.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use futures_util::{SinkExt, StreamExt};
use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const BIN: &str = env!("CARGO_BIN_EXE_fogar-agentd");
const KEY: &str = "stop-ledger-key";
const READY: &str = r#"{"type":"ready","session_id":"c:1","agent":"main","input_mode":"audio","output_mode":["audio","text"],"requested_capabilities":{},"live_capabilities":{}}"#;

fn state_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fogar-stop-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn store_key(dir: &Path) {
    let mut c = Command::new(BIN)
        .args(["store-key", "--state-dir"])
        .arg(dir)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(KEY.as_bytes()).unwrap();
    assert!(c.wait().unwrap().success());
}

/// Spawns the daemon and forwards its stderr lines to a channel.
fn spawn(dir: &Path, gateway: &str) -> (Child, mpsc::Receiver<String>) {
    let mut child = Command::new(BIN)
        .args(["run", "--no-ipc", "--gateway", gateway, "--state-dir"])
        .arg(dir)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let err = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for l in std::io::BufReader::new(err).lines() {
            let Ok(l) = l else { break };
            if tx.send(l).is_err() {
                break;
            }
        }
    });
    (child, rx)
}

fn wait_line(rx: &mpsc::Receiver<String>, needle: &str, all: &mut Vec<String>) {
    let t0 = Instant::now();
    loop {
        let left = Duration::from_secs(15).saturating_sub(t0.elapsed());
        let l = rx
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("no {needle:?} in {all:#?}"));
        all.push(l.clone());
        if l.contains(needle) {
            return;
        }
    }
}

fn term_and_wait(child: &mut Child) {
    let pid = child.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let t = Instant::now();
    let code = loop {
        if let Some(c) = child.try_wait().unwrap() {
            break c;
        }
        assert!(t.elapsed() < Duration::from_secs(6), "stop took > 6 s");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(code.success(), "{code:?}");
}

fn ledger_kinds(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("boot.ledger"))
        .unwrap()
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_owned))
        .collect()
}

#[test]
fn hung_gateway_close_still_records_a_clean_stop() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let port = listener.local_addr().unwrap().port();
    // Gateway: handshake, `ready`, then never reads again (the close is never answered).
    rt.spawn(async move {
        let mut held = Vec::new();
        loop {
            let Ok((s, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(s).await else {
                continue;
            };
            let _ = ws.next().await;
            let _ = ws.send(Message::Text(READY.into())).await;
            held.push(ws);
        }
    });
    let dir = state_dir("hung");
    store_key(&dir);
    let (mut child, rx) = spawn(&dir, &format!("ws://127.0.0.1:{port}/ws/stream"));
    let mut all = Vec::new();
    wait_line(&rx, "link ready", &mut all);
    term_and_wait(&mut child);
    all.extend(rx.try_iter());
    assert_eq!(ledger_kinds(&dir), ["start", "clean"], "{all:#?}");
    let _ = std::fs::remove_dir_all(&dir);
    rt.shutdown_background();
}

#[test]
fn sigterm_right_after_boot_records_a_clean_stop() {
    for i in 0..5 {
        let dir = state_dir(&format!("early{i}"));
        store_key(&dir);
        // Nothing listens on port 9: the link only backs off.
        let (mut child, rx) = spawn(&dir, "ws://127.0.0.1:9/ws/stream");
        let mut all = Vec::new();
        wait_line(&rx, "boot mode", &mut all);
        term_and_wait(&mut child);
        all.extend(rx.try_iter());
        assert_eq!(ledger_kinds(&dir), ["start", "clean"], "run {i}: {all:#?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Review sec#3: plain `ws://` (loopback only in slice 0) is allowed but announced loudly.
#[test]
fn plain_ws_gateway_prints_a_loud_warning() {
    let dir = state_dir("wswarn");
    store_key(&dir);
    let (mut child, rx) = spawn(&dir, "ws://127.0.0.1:9/ws/stream");
    let mut all = Vec::new();
    wait_line(&rx, "boot mode", &mut all);
    term_and_wait(&mut child);
    assert!(
        all.iter()
            .any(|l| l.contains("WARNING") && l.to_lowercase().contains("unencrypted")),
        "{all:#?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
