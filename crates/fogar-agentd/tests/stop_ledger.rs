//! Review code#2 / code#7b: an ordinary stop (SIGTERM) must always leave a `clean` line in the
//! boot ledger, even when the gateway never answers the close handshake and even when the signal
//! arrives right after boot; otherwise four ordinary stops trip SafeMode.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const BIN: &str = env!("CARGO_BIN_EXE_fogar-agentd");
const KEY: &str = "stop-ledger-key";
/// A `GET /events` answer up to its `ready` (close-delimited body: no length, no chunking).
const READY_STREAM: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\nretry: 1000\n\nid: 1\ndata: {\"type\":\"ready\",\"protocol\":5}\n\n";

/// Reads a request head (the daemon sends no body on `GET /events`). `false`: not a request.
async fn read_head(s: &mut TcpStream) -> bool {
    let mut buf = Vec::new();
    let mut b = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match s.read(&mut b).await {
            Ok(0) | Err(_) => return false,
            Ok(n) => buf.extend_from_slice(&b[..n]),
        }
        if buf.len() > 64 * 1024 {
            return false;
        }
    }
    true
}

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
    // Gateway: `/events` answers `ready`, then never sends nor reads again.
    rt.spawn(async move {
        let mut held = Vec::new();
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            if read_head(&mut s).await {
                let _ = s.write_all(READY_STREAM.as_bytes()).await;
                held.push(s);
            }
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

/// Round-2 review F3: the stderr rate limit must never swallow a terminal state. The gateway
/// flaps (`ready`, then a normal 1000 close) until the burst is used up, then revokes the key
/// (4200 -> the account is deleted, NotPaired; spec §0). The `link not_paired` line
/// must still appear, and the count of
/// suppressed lines must be reported (here: at the latest when the daemon stops).
#[test]
fn rate_limit_never_swallows_parked_and_reports_suppressed_lines() {
    const FLAPS: u32 = 7;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let port = listener.local_addr().unwrap().port();
    rt.spawn(async move {
        let mut n = 0u32;
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            if !read_head(&mut s).await {
                continue;
            }
            n += 1;
            // `ready`, then the stream ends at once (a flap); then the device is revoked (401).
            let reply = if n <= FLAPS {
                READY_STREAM.to_owned()
            } else {
                let body = r#"{"detail":"not_authenticated"}"#;
                format!(
                    "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
            let _ = s.write_all(reply.as_bytes()).await;
            let _ = s.shutdown().await;
        }
    });
    let dir = state_dir("ratelimit");
    store_key(&dir);
    let (mut child, rx) = spawn(&dir, &format!("ws://127.0.0.1:{port}/ws/stream"));
    // Lines with their arrival time (the backoff grows exponentially, so the flaps take ~30 s).
    let mut all: Vec<(Instant, String)> = Vec::new();
    let t0 = Instant::now();
    loop {
        let left = Duration::from_secs(150).saturating_sub(t0.elapsed());
        let Ok(l) = rx.recv_timeout(left) else {
            let _ = child.kill();
            panic!("no `link not_paired` line: {all:#?}");
        };
        let parked = l.contains("link not_paired");
        if parked {
            // The revocation names its close, like the backoff lines do.
            assert!(l.contains("(last close 401 -> Repair)"), "{l}");
        }
        all.push((Instant::now(), l));
        // Before the fix this line never came once the flaps had used up the burst.
        if parked {
            break;
        }
    }
    term_and_wait(&mut child);
    all.extend(rx.try_iter().map(|l| (Instant::now(), l)));
    let link: Vec<Instant> = all
        .iter()
        .filter(|(_, l)| l.contains("fogar-agentd: link "))
        .map(|(t, _)| *t)
        .collect();
    // The daemon's window is 60 s with a burst of 20; if 21 link lines came within 55 s of the
    // first (margin for pipe latency), lines were suppressed and the count must be reported.
    if link.len() > 20 && link[20].duration_since(link[0]) < Duration::from_secs(55) {
        assert!(
            all.iter()
                .any(|(_, l)| l.contains("link status lines suppressed")),
            "burst exceeded and no suppressed summary: {all:#?}"
        );
    } else {
        eprintln!("note: flaps spread over > 55 s; suppressed-summary assertion not applicable");
    }
    let _ = std::fs::remove_dir_all(&dir);
    rt.shutdown_background();
}
