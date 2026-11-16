//! Protocol-v1 mock gateway over a REAL local WebSocket (127.0.0.1, ephemeral port).

use super::{
    driver::{
        ClientIn, Ctl, GatewayLog, MockConfig, ServerOut, ServerTransport, SharedLog, run_conn,
    },
    http::{HttpRoutes, serve_http},
};
use futures_util::{SinkExt, StreamExt};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        protocol::{CloseFrame, frame::coding::CloseCode},
    },
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

#[derive(Debug)]
pub struct MockGateway {
    addr: SocketAddr,
    log: SharedLog,
    cfg: Arc<Mutex<MockConfig>>,
    ctls: Arc<Mutex<Vec<mpsc::Sender<Ctl>>>>,
    routes: HttpRoutes,
    cancel: CancellationToken,
    tasks: TaskTracker,
}

impl MockGateway {
    pub async fn start(cfg: MockConfig) -> std::io::Result<Self> {
        Self::from_listener(TcpListener::bind("127.0.0.1:0").await?, cfg)
    }

    /// Serves on an already-bound listener (e.g. a fixed port that was down a moment ago).
    pub fn from_listener(listener: TcpListener, cfg: MockConfig) -> std::io::Result<Self> {
        let addr = listener.local_addr()?;
        let gw = Self {
            addr,
            log: Arc::default(),
            cfg: Arc::new(Mutex::new(cfg)),
            ctls: Arc::default(),
            routes: HttpRoutes::fogar("mock"),
            cancel: CancellationToken::new(),
            tasks: TaskTracker::new(),
        };
        let (log, cfg, ctls, routes, cancel, tasks) = (
            gw.log.clone(),
            gw.cfg.clone(),
            gw.ctls.clone(),
            gw.routes.clone(),
            gw.cancel.clone(),
            gw.tasks.clone(),
        );
        gw.tasks.spawn(async move {
            loop {
                let (sock, _) = tokio::select! {
                    () = cancel.cancelled() => break,
                    r = listener.accept() => match r { Ok(v) => v, Err(_) => continue },
                };
                let (log, cfg, cancel) = (log.clone(), cfg.lock().unwrap().clone(), cancel.clone());
                let (ctls, routes) = (ctls.clone(), routes.clone());
                tasks.spawn(async move {
                    let _ = sock.set_nodelay(true);
                    // Same port as the real gateway: `/ws/...` upgrades, anything else is REST.
                    let ws = tokio::select! {
                        () = cancel.cancelled() => return,
                        ws = is_websocket(&sock) => ws,
                    };
                    if !ws {
                        tokio::select! {
                            () = cancel.cancelled() => {}
                            () = serve_http(sock, routes) => {}
                        }
                        return;
                    }
                    let (ctl_tx, ctl_rx) = mpsc::channel(8);
                    ctls.lock().unwrap().push(ctl_tx);
                    let Ok(ws) = tokio_tungstenite::accept_async(sock).await else {
                        return;
                    };
                    tokio::select! {
                        () = cancel.cancelled() => {}
                        () = run_conn(WsTransport { ws }, cfg, log, ctl_rx) => {}
                    }
                });
            }
        });
        Ok(gw)
    }

    /// `ws://127.0.0.1:<port>/ws/stream` (the real endpoint path, spec §1).
    pub fn url(&self) -> String {
        format!("ws://{}/ws/stream", self.addr)
    }
    /// The mock's address as the core and the link adapters see it.
    pub fn addr(&self) -> fogar_ports::gateway::GatewayAddr {
        fogar_ports::gateway::GatewayAddr::parse(&self.addr.to_string()).expect("mock addr")
    }
    /// The REST routes served on the same port (`/.well-known/fogar-gateway`, `POST /pair`).
    pub fn routes(&self) -> HttpRoutes {
        self.routes.clone()
    }
    pub fn log(&self) -> GatewayLog {
        self.log.lock().unwrap().clone()
    }
    /// Applies to connections accepted from now on.
    pub fn set_config(&self, cfg: MockConfig) {
        *self.cfg.lock().unwrap() = cfg;
    }
    pub fn control(&self, cmd: Ctl) -> usize {
        let mut ctls = self.ctls.lock().unwrap();
        ctls.retain(|c| !c.is_closed());
        ctls.iter()
            .filter(|c| c.try_send(cmd.clone()).is_ok())
            .count()
    }
    pub fn close_all(&self, code: u16, reason: &str) -> usize {
        self.control(Ctl::Close {
            code,
            reason: reason.to_owned(),
        })
    }
    pub async fn shutdown(self) {
        self.cancel.cancel();
        self.tasks.close();
        self.tasks.wait().await;
    }
}

/// Peeks (without consuming) until the request line is in: does it ask for `/ws/`? Bounded:
/// a client that never finishes its request line counts as HTTP and gets dropped there.
async fn is_websocket(sock: &TcpStream) -> bool {
    let mut buf = [0u8; 64];
    for _ in 0..500 {
        let n = match sock.peek(&mut buf).await {
            Ok(0) | Err(_) => return false,
            Ok(n) => n,
        };
        if buf[..n].contains(&b'\n') || n == buf.len() {
            return buf[..n].starts_with(b"GET /ws/");
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    false
}

struct WsTransport {
    ws: WebSocketStream<TcpStream>,
}

impl ServerTransport for WsTransport {
    async fn recv(&mut self) -> Option<ClientIn> {
        loop {
            return match self.ws.next().await? {
                Ok(Message::Text(t)) => Some(ClientIn::Text(t.as_str().to_owned())),
                Ok(Message::Binary(b)) => Some(ClientIn::Binary(b.to_vec())),
                Ok(Message::Close(_)) => Some(ClientIn::Closed),
                Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => continue,
                Err(_) => None,
            };
        }
    }
    async fn send(&mut self, out: ServerOut) -> bool {
        let m = match out {
            ServerOut::Text(t) => Message::Text(t.into()),
            ServerOut::Binary(b) => Message::Binary(b.into()),
        };
        self.ws.send(m).await.is_ok()
    }
    async fn close(&mut self, code: u16, reason: &str) {
        let frame = CloseFrame {
            code: CloseCode::from(code),
            reason: reason.into(),
        };
        if self.ws.close(Some(frame)).await.is_ok() {
            // Wait (bounded) for the client's close reply so the close handshake completes.
            let _ = tokio::time::timeout(Duration::from_secs(1), async {
                while self.ws.next().await.is_some() {}
            })
            .await;
        }
    }
}
