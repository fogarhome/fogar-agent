//! Canary for the IPC contract clause the subscription relies on (`recv` is cancelled every
//! time a push wins the `select!`): an adapter whose `recv` loses a half-read frame when
//! dropped MUST fail `recv_is_cancel_safe_around_pushes`. If this test ever passes, the clause
//! stopped checking anything and the core's subscribe loop is unprotected.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use fogar_ports::ipc::{
    IpcCaps, IpcConn, IpcError, IpcRequest, IpcResponse, IpcServer, PeerIdentity,
};
use fogar_testkit::contract::ipc_server::{IpcTestClient, recv_is_cancel_safe_around_pushes};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// A frame as the lossy "wire" carries it.
enum Part {
    Whole(IpcRequest),
    Half(IpcRequest),
    Rest,
}

struct LossyServer(Arc<Mutex<mpsc::Receiver<LossyConn>>>);

struct LossyConn {
    rx: mpsc::Receiver<Part>,
    tx: mpsc::Sender<IpcResponse>,
}

struct LossyClient {
    tx: mpsc::Sender<Part>,
    rx: mpsc::Receiver<IpcResponse>,
}

impl IpcConn for LossyConn {
    async fn recv(&mut self) -> Result<Option<IpcRequest>, IpcError> {
        match self.rx.recv().await {
            None => Ok(None),
            Some(Part::Whole(r)) => Ok(Some(r)),
            // NOT cancel-safe: the first half lives only in this future; dropping it here
            // (a push won the select) loses it.
            Some(Part::Half(r)) => match self.rx.recv().await {
                Some(Part::Rest) => Ok(Some(r)),
                _ => Err(IpcError::Malformed),
            },
            Some(Part::Rest) => Err(IpcError::Malformed),
        }
    }
    async fn send(&mut self, resp: IpcResponse) -> Result<(), IpcError> {
        self.tx
            .send(resp)
            .await
            .map_err(|_| IpcError::Transport("peer gone".into()))
    }
}

impl IpcServer for LossyServer {
    type Conn = LossyConn;
    fn capabilities(&self) -> IpcCaps {
        IpcCaps::NONE
    }
    async fn accept(&self) -> Result<(PeerIdentity, LossyConn), IpcError> {
        match self.0.lock().await.recv().await {
            Some(c) => Ok((PeerIdentity::uid_only(Some(0)), c)),
            None => std::future::pending().await,
        }
    }
}

impl IpcTestClient for LossyClient {
    async fn request(&mut self, r: IpcRequest) -> Option<IpcResponse> {
        self.tx.send(Part::Whole(r)).await.ok()?;
        self.rx.recv().await
    }
    async fn next(&mut self) -> Option<IpcResponse> {
        self.rx.recv().await
    }
    async fn send_half(&mut self, r: &IpcRequest, first: bool) {
        let part = if first {
            Part::Half(r.clone())
        } else {
            Part::Rest
        };
        self.tx.send(part).await.unwrap();
    }
}

#[test]
#[should_panic(expected = "cancel-safety")]
fn a_lossy_recv_fails_the_cancel_safety_clause() {
    fogar_testkit::run_async(async {
        let (conn_tx, conn_rx) = mpsc::channel(1);
        let server = LossyServer(Arc::new(Mutex::new(conn_rx)));
        let connect = || async move {
            let (req_tx, req_rx) = mpsc::channel(8);
            let (resp_tx, resp_rx) = mpsc::channel(8);
            conn_tx
                .send(LossyConn {
                    rx: req_rx,
                    tx: resp_tx,
                })
                .await
                .unwrap();
            LossyClient {
                tx: req_tx,
                rx: resp_rx,
            }
        };
        recv_is_cancel_safe_around_pushes(server, connect).await;
    });
}

/// The same clause passes for the in-memory fake the core tests use (else the canary above
/// would prove nothing).
#[test]
fn the_fake_passes_the_clause() {
    fogar_testkit::run_async(async {
        let ipc = fogar_testkit::FakeIpc::new();
        let i2 = ipc.clone();
        recv_is_cancel_safe_around_pushes(ipc, || async move { i2.client().await }).await;
    });
}
