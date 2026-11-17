//! Test rig: the agent wired to in-memory fakes (paused-time friendly).
#![allow(clippy::unwrap_used)]
#![allow(dead_code)]

use fogar_core::{
    Agent, AgentPorts, Config, HealthProbe, LinkState, LinkView, NoHttp, NoIpc, NoTailnet,
    PairingRecord, crashloop::BootMode,
};
use fogar_ports::{
    gateway::{GatewayAddr, GatewayLink},
    gateway_http::{GatewayHttp, HttpCall, HttpError, HttpReply},
    ipc::{IpcCaps, IpcError, IpcServer, PeerIdentity},
    secrets::{SecretKey, SecretStore},
};
use fogar_testkit::{
    FakeGatewayHttp, FakeGatewayLink, FakeIpc, FakeMic, FakeSpeaker, FakeSystemProbe, FakeTailnet,
    HttpRoutes, MemorySecretStore, MockConfig, SeededRandom, TestClock, silence_chunks,
    tone_chunks,
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
}

/// Address of the paired test gateway (the in-memory link ignores it; `FakeGatewayHttp`
/// answers on it).
pub(crate) const GW_ADDR: &str = "127.0.0.1:8004";

pub(crate) fn test_config() -> Config {
    let mut c = Config::default();
    c.vad.hangover_chunks = 10;
    c
}

/// A pairing record for `GW_ADDR` with `key`.
pub(crate) fn record(key: &str) -> PairingRecord {
    PairingRecord::new(
        GatewayAddr::parse(GW_ADDR).unwrap(),
        "client-1".into(),
        key.to_owned().into(),
        "green-house".into(),
        "1.29.0".into(),
        "Mac".into(),
    )
}

/// A store holding a pairing record with `key`.
pub(crate) fn paired_store(key: &str) -> MemorySecretStore {
    let s = MemorySecretStore::default();
    s.put(SecretKey::GatewayPairing, record(key).encode())
        .unwrap();
    s
}

/// `FakeIpc` whose peers the "adapter" verified as the same signer (`signed`) or by uid only.
#[derive(Debug, Clone)]
pub(crate) struct TestIpc {
    pub(crate) inner: FakeIpc,
    pub(crate) signed: bool,
}

impl IpcServer for TestIpc {
    type Conn = <FakeIpc as IpcServer>::Conn;
    fn capabilities(&self) -> IpcCaps {
        self.inner.capabilities()
    }
    async fn accept(&self) -> Result<(PeerIdentity, Self::Conn), IpcError> {
        let (peer, conn) = self.inner.accept().await?;
        let peer = if self.signed {
            PeerIdentity::verified_signer(peer.uid, "com.fogar.desktop".into())
        } else {
            peer
        };
        Ok((peer, conn))
    }
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
    async fn get(&self, addr: &GatewayAddr, call: HttpCall) -> Result<HttpReply, HttpError> {
        self.pick(addr).get(addr, call).await
    }
    async fn post_json(
        &self,
        addr: &GatewayAddr,
        call: HttpCall,
        body: secrecy::zeroize::Zeroizing<Vec<u8>>,
    ) -> Result<HttpReply, HttpError> {
        self.pick(addr).post_json(addr, call, body).await
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
            routes: HttpRoutes::fogar("green-house"),
            http_addrs: vec![GW_ADDR],
            tailnet: FakeTailnet::unsupported(),
        }
    }

    pub(crate) fn start(self) -> Rig<L> {
        let clock = TestClock::new();
        let ipc = self.with_ipc.then(FakeIpc::new);
        let ports = AgentPorts {
            clock: Arc::new(clock),
            link: self.link.clone(),
            ipc: ipc.clone().map(|inner| TestIpc {
                inner,
                signed: self.signed_peer,
            }),
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

/// Feeds one scripted utterance at real-time pace: `voiced` tone chunks, then enough silence
/// to end it (hangover 10).
pub(crate) async fn speak(mic: &FakeMic, voiced: usize) {
    for c in tone_chunks(voiced, 0.3)
        .into_iter()
        .chain(silence_chunks(14))
    {
        mic.push(c);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub(crate) fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}
