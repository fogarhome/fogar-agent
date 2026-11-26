//! Test rig: the agent wired to in-memory fakes (paused-time friendly).
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use fogar_core::{
    AccountRecord, Agent, AgentPorts, Config, HealthProbe, LinkState, LinkView, NoHttp, NoIpc,
    NoTailnet, crashloop::BootMode,
};
use fogar_ports::{
    gateway::{GatewayAddr, GatewayLink},
    gateway_http::{GatewayHttp, HttpCall, HttpError, HttpReply},
    ipc::{IpcRequest, IpcResponse, UserText},
    secrets::{SecretKey, SecretStore},
};
use fogar_testkit::{
    FakeGatewayHttp, FakeGatewayLink, FakeIpc, FakeMic, FakeSpeaker, FakeSystemProbe, FakeTailnet,
    HttpRoutes, MemorySecretStore, MockConfig, SeededRandom, TestClock,
};
use std::{sync::Arc, time::Duration};

pub(crate) struct Rig<L: GatewayLink = FakeGatewayLink> {
    pub(crate) agent: Agent<TestClock>,
    pub(crate) health: HealthProbe,
    pub(crate) link: L,
    pub(crate) mic: FakeMic,
    pub(crate) speaker: FakeSpeaker,
    pub(crate) clock: TestClock,
    pub(crate) secrets: Arc<MemorySecretStore>,
    pub(crate) ipc: Option<FakeIpc>,
    pub(crate) routes: HttpRoutes,
    /// The app with its conversation on screen (`RigBuilder::app_on_screen`).
    pub(crate) app: tokio::task::JoinSet<()>,
}

/// Address of the signed-in test gateway (the in-memory link ignores it; `FakeGatewayHttp`
/// answers on it).
pub(crate) const GW_ADDR: &str = "127.0.0.1:8004";

pub(crate) fn test_config() -> Config {
    Config::default()
}

/// An account at `GW_ADDR` with token `key`.
pub(crate) fn record(key: &str) -> AccountRecord {
    AccountRecord::new(
        GatewayAddr::parse(GW_ADDR).unwrap(),
        "client-1".into(),
        key.to_owned().into(),
    )
}

/// A store signed in with token `key`.
pub(crate) fn paired_store(key: &str) -> MemorySecretStore {
    let s = MemorySecretStore::default();
    s.put(SecretKey::GatewayAccount, record(key).encode())
        .unwrap();
    s
}

pub(crate) struct RigBuilder<L: GatewayLink> {
    pub(crate) cfg: Config,
    pub(crate) link: L,
    pub(crate) mic: FakeMic,
    pub(crate) speaker: FakeSpeaker,
    pub(crate) secrets: Arc<MemorySecretStore>,
    pub(crate) with_ipc: bool,
    /// IPC peers are verified as the same signer (else uid only).
    pub(crate) signed_peer: bool,
    /// The signed app is open with the conversation on screen: it confirms every turn it is
    /// shown (`TurnVisible`), so its audio may play (spec S2, single output point). Without
    /// it nothing sounds.
    pub(crate) app_on_screen: bool,
    pub(crate) routes: HttpRoutes,
    /// Addresses where a gateway answers `routes` (all share them).
    pub(crate) http_addrs: Vec<&'static str>,
    pub(crate) tailnet: FakeTailnet,
}

/// Several `FakeGatewayHttp`s, one per address (discovery may find more than one gateway).
/// An unknown address goes to the first one: it applies the cleartext policy, then answers
/// `Unreachable`. `http_addrs` must not be empty.
#[derive(Debug)]
pub(crate) struct MultiHttp(Vec<FakeGatewayHttp>);

impl MultiHttp {
    fn pick(&self, addr: &GatewayAddr) -> &FakeGatewayHttp {
        self.0
            .iter()
            .find(|h| h.addr() == *addr)
            .unwrap_or(&self.0[0])
    }
}

impl GatewayHttp for MultiHttp {
    async fn request(
        &self,
        addr: &GatewayAddr,
        call: &HttpCall,
        token: Option<&secrecy::SecretString>,
        body: Option<secrecy::zeroize::Zeroizing<Vec<u8>>>,
    ) -> Result<HttpReply, HttpError> {
        self.pick(addr).request(addr, call, token, body).await
    }

    type Body = <FakeGatewayHttp as GatewayHttp>::Body;

    async fn open_stream(
        &self,
        addr: &GatewayAddr,
        call: &HttpCall,
        token: &secrecy::SecretString,
        last_event_id: Option<&str>,
        body: Option<secrecy::zeroize::Zeroizing<Vec<u8>>>,
    ) -> Result<fogar_ports::gateway_http::StreamReply<Self::Body>, HttpError> {
        self.pick(addr)
            .open_stream(addr, call, token, last_event_id, body)
            .await
    }
}

impl RigBuilder<FakeGatewayLink> {
    pub(crate) fn fake(mock: MockConfig) -> Self {
        let key = mock.client_key.clone();
        Self::with_link(FakeGatewayLink::new(mock), &key)
    }
}

impl<L: GatewayLink + Clone> RigBuilder<L> {
    pub(crate) fn with_link(link: L, key: &str) -> Self {
        Self {
            cfg: test_config(),
            link,
            mic: FakeMic::healthy(),
            speaker: FakeSpeaker::healthy(),
            secrets: Arc::new(paired_store(key)),
            with_ipc: false,
            signed_peer: true,
            app_on_screen: false,
            routes: HttpRoutes::fogar(),
            http_addrs: vec![GW_ADDR],
            tailnet: FakeTailnet::unsupported(),
        }
    }

    pub(crate) fn start(self) -> Rig<L> {
        let clock = TestClock::new();
        let signed = self.signed_peer;
        let ipc = self.with_ipc.then(|| {
            if signed {
                FakeIpc::signed()
            } else {
                FakeIpc::new()
            }
        });
        let ports = AgentPorts {
            clock: Arc::new(clock),
            link: self.link.clone(),
            ipc: ipc.clone(),
            http: MultiHttp(
                self.http_addrs
                    .iter()
                    .map(|a| {
                        FakeGatewayHttp::at(self.routes.clone(), GatewayAddr::parse(a).unwrap())
                    })
                    .collect(),
            ),
            tailnet: self.tailnet,
            secrets: self.secrets.clone(),
            random: Arc::new(SeededRandom::new(7)),
            audio_in: Arc::new(self.mic.clone()),
            audio_out: Arc::new(self.speaker.clone()),
            probe: Some(Arc::new(FakeSystemProbe::healthy())),
        };
        let agent = Agent::start(ports, self.cfg, BootMode::Normal);
        let mut app = tokio::task::JoinSet::new();
        if self.app_on_screen {
            let ipc = ipc.clone().expect("app_on_screen needs with_ipc");
            assert!(signed, "only the signed app may confirm a turn");
            app.spawn(async move { app_confirming_turns(ipc.client().await).await });
        }
        Rig {
            health: agent.health(),
            agent,
            link: self.link,
            mic: self.mic,
            speaker: self.speaker,
            clock,
            secrets: self.secrets,
            ipc,
            routes: self.routes,
            app,
        }
    }
}

impl<L: GatewayLink> RigBuilder<L> {
    /// `with_ipc` + the app showing the conversation (it confirms every turn).
    pub(crate) fn app_on_screen(mut self) -> Self {
        self.with_ipc = true;
        self.app_on_screen = true;
        self
    }
}

/// What the real app does with its conversation on screen: subscribe, and confirm each turn
/// the agent says waits for it (`awaiting_visible`). Ends when the agent closes the connection.
async fn app_confirming_turns(mut c: fogar_testkit::FakeIpcClient) {
    async fn send(c: &mut fogar_testkit::FakeIpcClient, r: IpcRequest) {
        c.send_half(&r, true).await;
        c.send_half(&r, false).await;
    }
    send(&mut c, IpcRequest::SubscribeConversation).await;
    let mut confirmed = String::new();
    while let Some(m) = c.next().await {
        if let IpcResponse::Conversation(v) = m
            && let Some(id) = v.awaiting_visible
            && id != confirmed
        {
            send(
                &mut c,
                IpcRequest::TurnVisible {
                    turn_id: id.clone(),
                },
            )
            .await;
            confirmed = id;
        }
    }
}

/// `start_no_ipc` exists so tests without IPC do not need a FakeIpc type parameter.
pub(crate) fn start_no_ipc<L: GatewayLink + Clone>(
    b: RigBuilder<L>,
) -> (Agent<TestClock>, HealthProbe) {
    let clock = TestClock::new();
    let ports: AgentPorts<TestClock, L, NoIpc, NoHttp> = AgentPorts {
        clock: Arc::new(clock),
        link: b.link.clone(),
        ipc: None,
        http: NoHttp,
        tailnet: NoTailnet,
        secrets: b.secrets.clone(),
        random: Arc::new(SeededRandom::new(7)),
        audio_in: Arc::new(b.mic.clone()),
        audio_out: Arc::new(b.speaker.clone()),
        probe: None,
    };
    let agent = Agent::start(ports, b.cfg, BootMode::Normal);
    let h = agent.health();
    (agent, h)
}

pub(crate) async fn wait_ready(h: &mut HealthProbe) -> LinkView {
    within(
        Duration::from_secs(600),
        h.wait_link(|v| matches!(v.state, LinkState::Ready { .. })),
    )
    .await
}

pub(crate) async fn wait_state(
    h: &mut HealthProbe,
    pred: impl FnMut(&LinkView) -> bool,
) -> LinkView {
    within(Duration::from_secs(3600), h.wait_link(pred)).await
}

pub(crate) async fn within<F: std::future::Future>(d: Duration, f: F) -> F::Output {
    match tokio::time::timeout(d, f).await {
        Ok(v) => v,
        Err(_) => panic!("condition not reached within {d:?}"),
    }
}

/// Polls `cond` every 10 ms (virtual or real) for up to `d`.
pub(crate) async fn eventually(d: Duration, mut cond: impl FnMut() -> bool) {
    within(d, async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
}

/// The person types `text` in the app: a fresh signed IPC connection sends it (the rig needs
/// `with_ipc`; it stays open only for this request, so the idle close never matters).
pub(crate) async fn say<L: GatewayLink>(rig: &Rig<L>, text: &str) {
    let mut c = rig
        .ipc
        .as_ref()
        .expect("rig built without ipc")
        .client()
        .await;
    let r = c
        .request(IpcRequest::SendText {
            text: UserText(text.to_owned()),
        })
        .await;
    assert_eq!(r, Some(IpcResponse::TextAccepted));
}

pub(crate) fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}
