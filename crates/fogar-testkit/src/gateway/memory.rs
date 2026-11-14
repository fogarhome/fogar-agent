//! In-memory `GatewayLink` backed by the protocol-v1 driver. For paused-time tests.

use super::driver::{
    ClientIn, Ctl, GatewayLog, MockConfig, ServerOut, ServerTransport, SharedLog, run_conn,
};
use fogar_ports::gateway::{Frame, GatewayLink, LinkError, LinkEvent, LinkSession};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::task::TaskTracker;

/// Cloneable: give one clone to the agent, keep one to script and inspect.
#[derive(Debug, Clone)]
pub struct FakeGatewayLink {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    cfg: Mutex<MockConfig>,
    log: SharedLog,
    ctls: Mutex<Vec<mpsc::Sender<Ctl>>>,
    fail_connects: AtomicU32,
    uplink_delay: Arc<Mutex<Option<Duration>>>,
    tasks: TaskTracker,
}

impl FakeGatewayLink {
    pub fn new(cfg: MockConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                cfg: Mutex::new(cfg),
                log: Arc::default(),
                ctls: Mutex::default(),
                fail_connects: AtomicU32::new(0),
                uplink_delay: Arc::new(Mutex::new(None)),
                tasks: TaskTracker::new(),
            }),
        }
    }
    /// Any allowed address: the in-memory gateway ignores it (beyond the clear-text policy).
    pub fn addr(&self) -> fogar_ports::gateway::GatewayAddr {
        fogar_ports::gateway::GatewayAddr::parse("127.0.0.1").expect("addr")
    }
    pub fn log(&self) -> GatewayLog {
        self.inner.log.lock().unwrap().clone()
    }
    /// The next `n` connects fail at the transport level.
    pub fn fail_next_connects(&self, n: u32) {
        self.inner.fail_connects.store(n, Ordering::SeqCst);
    }
    /// Every client `send` takes this long (a slow uplink). Applies to live sessions too.
    pub fn set_uplink_delay(&self, d: Option<Duration>) {
        *self.inner.uplink_delay.lock().unwrap() = d;
    }
    pub fn set_config(&self, cfg: MockConfig) {
        *self.inner.cfg.lock().unwrap() = cfg;
    }
    /// Sends a command to every live connection; returns how many received it.
    pub fn control(&self, cmd: Ctl) -> usize {
        let mut ctls = self.inner.ctls.lock().unwrap();
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
    /// Server-side tasks still running (they end when the client side is dropped).
    pub fn live_server_tasks(&self) -> usize {
        self.inner.tasks.len()
    }
}

struct MemTransport {
    rx: mpsc::Receiver<ClientIn>,
    tx: mpsc::Sender<LinkEvent>,
}

impl ServerTransport for MemTransport {
    async fn recv(&mut self) -> Option<ClientIn> {
        self.rx.recv().await
    }
    async fn send(&mut self, out: ServerOut) -> bool {
        let f = match out {
            ServerOut::Text(t) => Frame::Text(t),
            ServerOut::Binary(b) => Frame::Binary(b),
        };
        self.tx.send(LinkEvent::Frame(f)).await.is_ok()
    }
    async fn close(&mut self, code: u16, reason: &str) {
        let _ = self
            .tx
            .send(LinkEvent::Closed {
                code,
                reason: reason.to_owned(),
            })
            .await;
    }
}

/// Client end of an in-memory session.
#[derive(Debug)]
pub struct FakeSession {
    tx: mpsc::Sender<ClientIn>,
    rx: mpsc::Receiver<LinkEvent>,
    closed: bool,
    delay: Arc<Mutex<Option<Duration>>>,
}

pub const FAKE_MAX_FRAME: usize = fogar_proto::limits::WS_MAX_FRAME_BYTES;

impl LinkSession for FakeSession {
    fn max_frame(&self) -> usize {
        FAKE_MAX_FRAME
    }
    async fn send(&mut self, frame: Frame) -> Result<(), LinkError> {
        if self.closed {
            return Err(LinkError::Disconnected);
        }
        if frame.len() > FAKE_MAX_FRAME {
            return Err(LinkError::TooLarge {
                len: frame.len(),
                max: FAKE_MAX_FRAME,
            });
        }
        let delay = *self.delay.lock().unwrap();
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
        let m = match frame {
            Frame::Text(t) => ClientIn::Text(t),
            Frame::Binary(b) => ClientIn::Binary(b),
        };
        self.tx.send(m).await.map_err(|_| LinkError::Disconnected)
    }
    async fn recv(&mut self) -> Option<LinkEvent> {
        self.rx.recv().await
    }
    async fn close(&mut self, _code: u16, _reason: &str) -> Result<(), LinkError> {
        if !self.closed {
            self.closed = true;
            let _ = self.tx.try_send(ClientIn::Closed);
        }
        Ok(())
    }
}

impl GatewayLink for FakeGatewayLink {
    type Session = FakeSession;
    async fn connect(
        &self,
        addr: &fogar_ports::gateway::GatewayAddr,
    ) -> Result<FakeSession, LinkError> {
        if !super::http::policy_allows(addr) {
            return Err(LinkError::NotConfigured);
        }
        let fail = self
            .inner
            .fail_connects
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if fail {
            return Err(LinkError::Transport("scripted connect failure".into()));
        }
        let (c2s_tx, c2s_rx) = mpsc::channel(64);
        let (s2c_tx, s2c_rx) = mpsc::channel(256);
        let (ctl_tx, ctl_rx) = mpsc::channel(8);
        self.inner.ctls.lock().unwrap().push(ctl_tx);
        let cfg = self.inner.cfg.lock().unwrap().clone();
        self.inner.tasks.spawn(run_conn(
            MemTransport {
                rx: c2s_rx,
                tx: s2c_tx,
            },
            cfg,
            self.inner.log.clone(),
            ctl_rx,
        ));
        Ok(FakeSession {
            tx: c2s_tx,
            rx: s2c_rx,
            closed: false,
            delay: self.inner.uplink_delay.clone(),
        })
    }
}
