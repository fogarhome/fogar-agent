//! Manual check of the same-signer peer check with separately signed processes (ADR-0002
//! "To verify on the M2"). `cargo test` binaries are ad-hoc signed, so only this covers the
//! signed paths. Recipe (results recorded in ADR-0002):
//!
//! ```text
//! cargo build -p fogar-agent-macos --example signer_probe
//! cp target/debug/examples/signer_probe agent && cp agent app && cp agent adhoc
//! codesign -f -s "<Apple Development identity>" -i com.fogar.agent agent
//! codesign -f -s "<Apple Development identity>" -o runtime -i com.fogar.desktop app
//! codesign -f -s - -i com.fogar.desktop adhoc
//! ./agent server /tmp/fp & sleep 0.3; ./app client /tmp/fp; wait      # SameSigner
//! ./agent server /tmp/fp & sleep 0.3; ./adhoc client /tmp/fp; wait    # UidOnly
//! ```
//!
//! fd-sharing attack (ADR-0002, `tests/fixtures/fd_share_attack.py`): an unsigned script connects, then forks + execs `./app idle`;
//! the child (mode `child`) or the parent before the exec (mode `pre`) writes a `Pair` frame.
//! Expected: `peer SameSigner` then `recv REFUSED` (never `recv DELIVERED Pair`).
//!
//! `server <dir> [peer-identifier]` accepts ONE connection after 1 s (so the measured `accept`
//! time is the peer check alone), prints caps, the verdict and what each request became.
//! `client <dir>` sends `Hello`, then `Pair` after the reply. `idle` just holds inherited fds.

#[cfg(target_os = "macos")]
fn main() {
    use fogar_agent_macos::{COMPANION_APP_ID, MacIpcClient, MacIpcServer};
    use fogar_ports::ipc::{IpcConn, IpcRequest, IpcResponse, IpcServer};
    use std::{path::PathBuf, time::Duration};

    let args: Vec<String> = std::env::args().collect();
    let (Some(mode), Some(dir)) = (args.get(1).cloned(), args.get(2).map(PathBuf::from)) else {
        eprintln!("usage: signer_probe server|client|idle <dir> [peer-identifier]");
        std::process::exit(2);
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| panic!("runtime: {e}"));
    rt.block_on(async {
        match mode.as_str() {
            "server" => {
                let id = args.get(3).map_or(COMPANION_APP_ID, String::as_str);
                let s = MacIpcServer::bind_for(&dir, id).unwrap_or_else(|e| panic!("bind: {e}"));
                println!("caps {:?}", s.capabilities());
                tokio::time::sleep(Duration::from_secs(1)).await;
                let t = std::time::Instant::now();
                let (peer, mut conn) = s.accept().await.unwrap_or_else(|e| panic!("accept: {e}"));
                println!(
                    "peer {:?} code_id {:?} uid {:?} check_ms {:.1}",
                    peer.check(),
                    peer.code_id(),
                    peer.uid,
                    t.elapsed().as_secs_f64() * 1e3
                );
                // Serve until the peer closes; print what each request turned into.
                loop {
                    match conn.recv().await {
                        Ok(Some(IpcRequest::Hello { version })) => {
                            println!("recv Hello");
                            let _ = conn.send(IpcResponse::Hello { version }).await;
                        }
                        Ok(Some(r)) => {
                            println!("recv DELIVERED {r:?}");
                            let _ = conn.send(IpcResponse::Unpaired).await;
                        }
                        Ok(None) => {
                            println!("recv closed");
                            break;
                        }
                        Err(e) => {
                            println!("recv REFUSED {e}");
                            break;
                        }
                    }
                }
            }
            // Holds whatever fds it inherited and does nothing (the exec target of the attack).
            "idle" => tokio::time::sleep(Duration::from_secs(4)).await,
            "client" => {
                let mut c = MacIpcClient::connect(&dir.join("agent.sock"))
                    .await
                    .unwrap_or_else(|e| panic!("connect: {e}"));
                println!(
                    "reply {:?}",
                    c.request(IpcRequest::Hello { version: 2 }).await
                );
                // A privileged request, sent only after the Hello was answered (as the app must).
                let pair = IpcRequest::Pair {
                    address: "green-house:8004".into(),
                    code: fogar_ports::ipc::PairCode::parse("482913"),
                };
                println!("reply {:?}", c.request(pair).await);
            }
            _ => std::process::exit(2),
        }
    });
}

#[cfg(not(target_os = "macos"))]
fn main() {}
