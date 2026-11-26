use fogar_ports::{
    Support,
    ipc::{IpcCaps, IpcConn, IpcError, IpcRequest, IpcResponse, IpcServer, PeerIdentity},
};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// In-memory IPC server (cloneable handle). `client()` opens a connection the next `accept`
/// returns. Peers are uid-only (`new`) or verified as the same signer (`signed`, IPC-4).
#[derive(Debug, Clone)]
pub struct FakeIpc {
    tx: mpsc::Sender<FakeConn>,
    rx: Arc<Mutex<mpsc::Receiver<FakeConn>>>,
    signed: bool,
}

#[derive(Debug)]
pub struct FakeConn {
    rx: mpsc::Receiver<IpcRequest>,
    tx: mpsc::Sender<IpcResponse>,
}

/// Test-side end of a fake IPC connection.
#[derive(Debug)]
pub struct FakeIpcClient {
    tx: mpsc::Sender<IpcRequest>,
    rx: mpsc::Receiver<IpcResponse>,
    half: Option<IpcRequest>,
}

impl FakeIpc {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(4);
        Self {
            tx,
            rx: Arc::new(Mutex::new(rx)),
            signed: false,
        }
    }
    /// Peers count as the agent's own signed app (`com.fogar.desktop`): may Login/Logout.
    pub fn signed() -> Self {
        Self {
            signed: true,
            ..Self::new()
        }
    }
    pub async fn client(&self) -> FakeIpcClient {
        let (req_tx, req_rx) = mpsc::channel(8);
        let (resp_tx, resp_rx) = mpsc::channel(8);
        self.tx
            .send(FakeConn {
                rx: req_rx,
                tx: resp_tx,
            })
            .await
            .expect("ipc server gone");
        FakeIpcClient {
            tx: req_tx,
            rx: resp_rx,
            half: None,
        }
    }
}

impl Default for FakeIpc {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeIpcClient {
    pub async fn request(&mut self, r: IpcRequest) -> Option<IpcResponse> {
        self.tx.send(r).await.ok()?;
        self.rx.recv().await
    }
    /// Split write: the request is delivered only when the second half is "written".
    pub async fn send_half(&mut self, r: &IpcRequest, first: bool) {
        if first {
            self.half = Some(r.clone());
        } else if let Some(r) = self.half.take() {
            self.tx.send(r).await.expect("ipc server gone");
        }
    }
    /// Next pushed message (after `Subscribe`). `None` = the server closed.
    pub async fn next(&mut self) -> Option<IpcResponse> {
        self.rx.recv().await
    }
}

impl IpcConn for FakeConn {
    async fn recv(&mut self) -> Result<Option<IpcRequest>, IpcError> {
        Ok(self.rx.recv().await)
    }
    async fn send(&mut self, resp: IpcResponse) -> Result<(), IpcError> {
        self.tx
            .send(resp)
            .await
            .map_err(|_| IpcError::Transport("peer gone".into()))
    }
}

impl IpcServer for FakeIpc {
    type Conn = FakeConn;
    fn capabilities(&self) -> IpcCaps {
        let mut c = IpcCaps::NONE;
        c.peer_uid = Support::Emulated;
        c
    }
    async fn accept(&self) -> Result<(PeerIdentity, FakeConn), IpcError> {
        // Cancel-safe: the tokio Mutex guard is only held across the cancel-safe recv; a dropped
        // accept loses no connection.
        let mut rx = self.rx.lock().await;
        match rx.recv().await {
            Some(c) if self.signed => Ok((
                PeerIdentity::verified_signer(Some(0), "com.fogar.desktop".into()),
                c,
            )),
            Some(c) => Ok((PeerIdentity::uid_only(Some(0)), c)),
            None => std::future::pending().await,
        }
    }
}
