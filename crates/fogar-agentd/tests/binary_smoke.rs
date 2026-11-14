//! Runs the real `fogar-agentd` binary (composition root) against the mock gateway: provision the
//! key, connect, answer a status request over the Unix socket, SIGTERM, clean exit, ledger.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use fogar_agent_linux::UdsIpcClient;
use fogar_ports::ipc::{IpcRequest, IpcResponse};
use fogar_testkit::{MockConfig, MockGateway};
use std::{
    io::Write,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_fogar-agentd");

#[test]
fn daemon_connects_serves_status_and_stops_cleanly_on_sigterm() {
    fogar_testkit::run_async(async {
        let gw = MockGateway::start(MockConfig::default()).await.unwrap();
        let dir = std::env::temp_dir().join(format!("fogar-smoke-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut store = Command::new(BIN)
            .args(["store-key", "--state-dir"])
            .arg(&dir)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        store
            .stdin
            .take()
            .unwrap()
            .write_all(MockConfig::default().client_key.as_bytes())
            .unwrap();
        assert!(store.wait().unwrap().success());

        let mut child = Command::new(BIN)
            .args(["run", "--gateway", &gw.url(), "--state-dir"])
            .arg(&dir)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let t0 = Instant::now();
        while gw.log().handshakes_ok == 0 {
            assert!(
                t0.elapsed() < Duration::from_secs(10),
                "daemon never completed the handshake"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let sock = dir.join("run").join("agent.sock");
        let mut status = None;
        while t0.elapsed() < Duration::from_secs(10) {
            if let Ok(mut c) = UdsIpcClient::connect(&sock).await
                && let Some(IpcResponse::Status(s)) = c.request(IpcRequest::Status).await
                && s.link == "ready"
            {
                status = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let status = status.expect("status over the Unix socket");
        assert_eq!((status.sessions, status.boot_mode.as_str()), (1, "normal"));

        let pid = child.id().to_string();
        assert!(
            Command::new("kill")
                .args(["-TERM", &pid])
                .status()
                .unwrap()
                .success()
        );
        let t1 = Instant::now();
        let code = loop {
            if let Some(code) = child.try_wait().unwrap() {
                break code;
            }
            assert!(
                t1.elapsed() < Duration::from_secs(5),
                "SIGTERM shutdown took > 5 s"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(code.success(), "{code:?}");
        let mut err = String::new();
        std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut err).unwrap();
        assert!(err.contains("clean: true"), "{err}");
        assert!(
            !err.contains(&MockConfig::default().client_key),
            "the client key must never be logged"
        );

        let t2 = Instant::now();
        while gw.log().client_closes == 0 {
            assert!(
                t2.elapsed() < Duration::from_secs(5),
                "no close frame from the daemon"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let ledger = std::fs::read_to_string(dir.join("boot.ledger")).unwrap();
        let kinds: Vec<_> = ledger
            .lines()
            .filter_map(|l| l.split_whitespace().nth(1))
            .collect();
        assert_eq!(kinds, ["start", "clean"]);
        assert!(!sock.exists(), "socket removed on exit");
        gw.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    });
}
