//! Composition of the engine services + ordered shutdown (fogar-longrun-reliability "Shutdown
//! ordering"): one `TaskTracker` and one token per phase.
//!
//! | Phase tracker | Tasks | Token |
//! |---|---|---|
//! | `data` | engine | `ingress` |
//! | `edge` | IPC service (+ its per-connection JoinSet) | `ingress` |
//! | `io` | link supervisor (`/events`) | `egress` |
//! | `ops` | liveness heartbeat | `ops` |
//!
//! Shutdown: (1) cancel `ingress` -> engine stops playback, IPC stops; wait `data`+`edge`
//! (budget/4). (2) cancel `egress` -> the supervisor drops its stream (the connection closes, no
//! handshake) and stops; wait `io` (the rest minus budget/4, so >= budget/2).
//! (3) cancel `ops`; wait (budget/4). Each wait has its own deadline; the report says what overran.
//! Invariant: `Config::close_timeout` < budget/2 (1 s vs 2 s for the daemon's 4 s budget), kept
//! for the voice link of PR 5.

use crate::{
    account::Account,
    capture::CaptureGate,
    config::Config,
    conversation::Chat,
    crashloop::BootMode,
    engine::Engine,
    heartbeat::Heartbeat,
    link::{Inbound, LinkCtx, LinkView},
    playback::PlaybackGate,
    queues,
    stats::{Health, LinkState, Stats},
    time::deadline,
};
use fogar_ports::{
    audio::{AudioSink, AudioSource},
    clock::Clock,
    gateway::{GatewayAddr, GatewayLink},
    gateway_http::{GatewayHttp, HttpCall, HttpError, HttpReply},
    ipc::{
        ConversationView, IpcCaps, IpcConn, IpcError, IpcRequest, IpcResponse, IpcServer,
        PeerIdentity,
    },
    random::Random,
    secrets::SecretStore,
    system_probe::{ProbeCaps, SystemProbe},
    tailnet::{TailnetError, TailnetPeers},
};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

/// Everything the engine needs from the outside world, built by the composition root.
#[allow(missing_debug_implementations)] // holds trait objects without Debug
pub struct AgentPorts<C, L, I, H = NoHttp, T = NoTailnet> {
    pub clock: Arc<C>,
    /// The voice WebSocket. Not used since S4 PR 4a (the chat goes over `http`); kept for the
    /// voice link of PR 5.
    pub link: L,
    /// `None` = no local UI socket on this platform/build.
    pub ipc: Option<I>,
    /// The gateway over HTTP: discovery and the account (IPC service), and the chat link
    /// (`GET /events`).
    pub http: H,
    /// Tailnet peers for discovery; `NoTailnet` where there is no adapter.
    pub tailnet: T,
    pub secrets: Arc<dyn SecretStore>,
    pub random: Arc<dyn Random>,
    pub audio_in: Arc<dyn AudioSource>,
    pub audio_out: Arc<dyn AudioSink>,
    pub probe: Option<Arc<dyn SystemProbe>>,
}

/// Cheap, cloneable view used by the IPC service, the composition root and tests.
#[derive(Debug, Clone)]
pub struct HealthProbe {
    pub(crate) stats: Arc<Stats>,
    pub(crate) view: watch::Receiver<LinkView>,
    pub(crate) boot_mode: &'static str,
    /// The conversation view the engine publishes (also the voice state for `StatusReport`).
    pub(crate) conversation: watch::Receiver<ConversationView>,
    /// The conversation store: the list and the open conversation (IPC v6 shows them, Task 5).
    chat: watch::Receiver<Chat>,
    probe_caps: Option<ProbeCaps>,
    in_tx: mpsc::WeakSender<Inbound>,
}

impl HealthProbe {
    pub fn snapshot(&self) -> Health {
        let v = self.view.borrow().clone();
        Health {
            link: v.state,
            last_close: v.last_close,
            stats: self.stats.snapshot(),
            boot_mode: self.boot_mode,
            in_queue_len: queue_len(&self.in_tx),
        }
    }

    /// Latest conversation view (for the composition root and tests).
    pub fn conversation(&self) -> ConversationView {
        self.conversation.borrow().clone()
    }

    /// Latest conversation store (the list, the open conversation).
    pub fn chat(&self) -> Chat {
        self.chat.borrow().clone()
    }

    pub fn probe_caps(&self) -> Option<ProbeCaps> {
        self.probe_caps
    }

    /// Resolves when the link state satisfies `pred` (cancel-safe; for the composition root's
    /// status logging and for tests).
    pub async fn wait_link(&mut self, mut pred: impl FnMut(&LinkView) -> bool) -> LinkView {
        loop {
            {
                let v = self.view.borrow_and_update();
                if pred(&v) {
                    return v.clone();
                }
            }
            if self.view.changed().await.is_err() {
                return self.view.borrow().clone();
            }
        }
    }
}

fn queue_len<T>(w: &mpsc::WeakSender<T>) -> usize {
    w.upgrade()
        .map_or(0, |tx| tx.max_capacity() - tx.capacity())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Every phase finished inside its deadline.
    pub clean: bool,
    pub ingress_ok: bool,
    pub egress_ok: bool,
    pub ops_ok: bool,
    /// Tasks still tracked after the deadlines (0 when clean).
    pub tasks_left: usize,
}

#[derive(Debug)]
pub struct Agent<C> {
    clock: Arc<C>,
    ingress: CancellationToken,
    egress: CancellationToken,
    ops_token: CancellationToken,
    data: TaskTracker,
    edge: TaskTracker,
    io: TaskTracker,
    ops: TaskTracker,
    health: HealthProbe,
    heartbeat: Arc<Heartbeat>,
    view_tx: watch::Sender<LinkView>,
    /// In-crate tests drive engine commands IPC v5 has no request for (opening).
    #[cfg(test)]
    pub(crate) cmds: mpsc::Sender<crate::engine::Command>,
}

impl<C: Clock> Agent<C> {
    /// Spawns the services on the current tokio runtime. Must be called inside a runtime.
    pub fn start<L, I, H, T>(ports: AgentPorts<C, L, I, H, T>, cfg: Config, boot: BootMode) -> Self
    where
        L: GatewayLink,
        I: IpcServer,
        H: GatewayHttp,
        T: TailnetPeers,
    {
        let cfg = Arc::new(cfg);
        let stats = Arc::new(Stats::default());
        let (view_tx, view_rx) = watch::channel(LinkView {
            state: LinkState::Starting,
            last_close: None,
            gateway: None,
        });
        // Account generation (`queues`: watch, coalescing): IPC login/logout -> link.
        let (account_tx, account_rx) = watch::channel(());
        // IPC reconnect -> parked link (`queues`: watch, coalescing).
        let (reconnect_tx, reconnect_rx) = watch::channel(());
        let (in_tx, in_rx) = mpsc::channel::<Inbound>(queues::INBOUND);
        let (cmd_tx, cmd_rx) = mpsc::channel(queues::ENGINE_COMMANDS);
        #[cfg(test)]
        let cmds = cmd_tx.clone();
        let (conv_tx, conv_rx) = watch::channel(ConversationView::default());
        // The conversation store (`queues`: watch, coalescing).
        let (chat_tx, chat_rx) = watch::channel(Chat::default());
        let health = HealthProbe {
            stats: stats.clone(),
            view: view_rx,
            boot_mode: boot.label(),
            conversation: conv_rx,
            chat: chat_rx,
            probe_caps: ports.probe.as_ref().map(|p| p.capabilities()),
            in_tx: in_tx.downgrade(),
        };
        let root = CancellationToken::new();
        let ingress = root.child_token();
        let egress = root.child_token();
        let ops_token = root.child_token();
        let (data, edge, io, ops) = (
            TaskTracker::new(),
            TaskTracker::new(),
            TaskTracker::new(),
            TaskTracker::new(),
        );
        let heartbeat = Arc::new(Heartbeat::new(ms(&*ports.clock)));

        let http = Arc::new(ports.http);
        let engine = Engine {
            clock: ports.clock.clone(),
            cfg: cfg.clone(),
            stats: stats.clone(),
            http: http.clone(),
            in_rx,
            cmd_rx,
            conv: conv_tx,
            chat: chat_tx,
        };
        // The microphone goes straight to the consent gate: the engine never holds it.
        let capture = CaptureGate::new(ports.audio_in, &cfg, stats.clone());
        // The speaker goes straight to the output gate: nothing sounds unseen (S2 3c).
        let playback = PlaybackGate::new(ports.audio_out, stats.clone());
        data.spawn(engine.run(capture, playback, ingress.clone()));

        let link = LinkCtx {
            clock: ports.clock.clone(),
            http: http.clone(),
            secrets: ports.secrets.clone(),
            random: ports.random,
            cfg: cfg.clone(),
            stats: stats.clone(),
            view: view_tx.clone(),
            budget: queues::ByteBudget::new(queues::INBOUND_BYTES),
            account: account_rx,
            _account_keep: account_tx.clone(),
            reconnect: reconnect_rx,
            _reconnect_keep: reconnect_tx.clone(),
        };
        io.spawn(link.run(in_tx, egress.clone()));

        if let Some(server) = ports.ipc {
            let account = Account {
                clock: ports.clock.clone(),
                http,
                tailnet: Arc::new(ports.tailnet),
                secrets: ports.secrets,
                cfg: cfg.clone(),
                stats: stats.clone(),
                changed: account_tx,
                reconnect: reconnect_tx,
                lock: Arc::new(Semaphore::new(queues::ACCOUNT)),
            };
            edge.spawn(crate::ipc::run(
                server,
                ports.clock.clone(),
                health.clone(),
                crate::ipc::Limits {
                    write_timeout: cfg.write_timeout,
                    idle_timeout: cfg.ipc_idle_timeout,
                    pair_from_uid_only_peers: cfg.pair_from_uid_only_peers,
                    permission_wait: cfg.mic_permission_wait,
                },
                account,
                cmd_tx,
                ingress.clone(),
            ));
        }

        ops.spawn(beat(
            ports.clock.clone(),
            heartbeat.clone(),
            cfg.heartbeat_every,
            ops_token.clone(),
        ));

        for t in [&data, &edge, &io, &ops] {
            t.close();
        }
        Self {
            clock: ports.clock,
            ingress,
            egress,
            ops_token,
            data,
            edge,
            io,
            ops,
            health,
            heartbeat,
            view_tx,
            #[cfg(test)]
            cmds,
        }
    }

    pub fn health(&self) -> HealthProbe {
        self.health.clone()
    }

    pub fn heartbeat(&self) -> Arc<Heartbeat> {
        self.heartbeat.clone()
    }

    /// Tasks owned by the agent's trackers (leak probe; IPC per-connection tasks are counted by
    /// the runtime metrics instead).
    pub fn live_tasks(&self) -> usize {
        self.data.len() + self.edge.len() + self.io.len() + self.ops.len()
    }

    /// Ordered shutdown within `budget` (aim < 5 s, below launchd ExitTimeOut).
    pub async fn shutdown(self, budget: Duration) -> ShutdownReport {
        self.view_tx
            .send_modify(|v| v.state = LinkState::ShuttingDown);
        // Phase split (review code#2): ingress and ops get a quarter each; egress gets the
        // rest (>= half), room for PR 5's WebSocket close handshake, bounded by
        // `Config::close_timeout` (1 s default) so a hung peer still fits the budget.
        let started = self.clock.now();
        let quarter = budget / 4;
        self.ingress.cancel();
        let ingress_ok = self
            .wait(quarter, async {
                self.data.wait().await;
                self.edge.wait().await;
            })
            .await;
        self.egress.cancel();
        let spent = self.clock.now().saturating_since(started);
        let egress_phase = budget.saturating_sub(spent).saturating_sub(quarter);
        let egress_ok = self.wait(egress_phase, self.io.wait()).await;
        self.ops_token.cancel();
        let ops_ok = self.wait(quarter, self.ops.wait()).await;
        self.view_tx.send_modify(|v| v.state = LinkState::Stopped);
        let tasks_left = self.live_tasks();
        ShutdownReport {
            clean: ingress_ok && egress_ok && ops_ok && tasks_left == 0,
            ingress_ok,
            egress_ok,
            ops_ok,
            tasks_left,
        }
    }

    async fn wait(&self, d: Duration, f: impl Future<Output = ()>) -> bool {
        deadline(&*self.clock, d, f).await.is_some()
    }
}

fn ms<C: Clock>(clock: &C) -> u64 {
    u64::try_from(clock.now().0.as_millis()).unwrap_or(u64::MAX)
}

async fn beat<C: Clock>(
    clock: Arc<C>,
    hb: Arc<Heartbeat>,
    every: Duration,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            () = clock.sleep(every) => hb.beat(ms(&*clock)),
        }
    }
}

/// In-crate tests: the agent over in-memory fakes (the ports are handed over here only, archtest
/// R11/R12).
#[cfg(test)]
pub(crate) fn start_for_test(
    http: fogar_testkit::FakeGatewayHttp,
    secrets: Arc<dyn SecretStore>,
    mic: fogar_testkit::FakeMic,
    speaker: fogar_testkit::FakeSpeaker,
) -> Agent<fogar_testkit::TestClock> {
    let ports: AgentPorts<_, _, NoIpc, _, NoTailnet> = AgentPorts {
        clock: Arc::new(fogar_testkit::TestClock::new()),
        link: fogar_testkit::FakeGatewayLink::new(fogar_testkit::MockConfig::default()),
        ipc: None,
        http,
        tailnet: NoTailnet,
        secrets,
        random: Arc::new(fogar_testkit::SeededRandom::new(7)),
        audio_in: Arc::new(mic),
        audio_out: Arc::new(speaker),
        probe: None,
    };
    Agent::start(ports, Config::default(), BootMode::Normal)
}

/// For builds without a local UI socket: `accept` never resolves.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoIpc;

/// Uninhabited connection type of [`NoIpc`].
#[derive(Debug)]
pub enum NoConn {}

impl IpcConn for NoConn {
    async fn recv(&mut self) -> Result<Option<IpcRequest>, IpcError> {
        match *self {}
    }
    async fn send(&mut self, _resp: IpcResponse) -> Result<(), IpcError> {
        match *self {}
    }
}

impl IpcServer for NoIpc {
    type Conn = NoConn;
    fn capabilities(&self) -> IpcCaps {
        IpcCaps::NONE
    }
    async fn accept(&self) -> Result<(PeerIdentity, NoConn), IpcError> {
        std::future::pending().await
    }
}

/// For platforms without a tailnet adapter: discovery skips the tailnet step.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoTailnet;

impl TailnetPeers for NoTailnet {
    fn capabilities(&self) -> fogar_ports::Support {
        fogar_ports::Support::Unsupported
    }
    async fn peers(&self) -> Result<Vec<std::net::IpAddr>, TailnetError> {
        Err(TailnetError::Unavailable)
    }
}

/// For builds without the HTTP adapter yet: nothing ever answers (discovery finds nothing,
/// sign-in fails with `Unreachable`).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHttp;

impl GatewayHttp for NoHttp {
    async fn request(
        &self,
        _addr: &GatewayAddr,
        _call: &HttpCall,
        _token: Option<&secrecy::SecretString>,
        _body: Option<secrecy::zeroize::Zeroizing<Vec<u8>>>,
    ) -> Result<HttpReply, HttpError> {
        Err(HttpError::Unreachable)
    }

    type Body = std::convert::Infallible;

    async fn open_stream(
        &self,
        _addr: &GatewayAddr,
        _call: &HttpCall,
        _token: &secrecy::SecretString,
        _last_event_id: Option<&str>,
        _body: Option<secrecy::zeroize::Zeroizing<Vec<u8>>>,
    ) -> Result<fogar_ports::gateway_http::StreamReply<Self::Body>, HttpError> {
        Err(HttpError::Unreachable)
    }
}
