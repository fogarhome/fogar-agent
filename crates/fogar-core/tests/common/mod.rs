//! Test rig: the agent wired to in-memory fakes (paused-time friendly).
#![allow(dead_code)]

use fogar_core::{
    Agent, AgentPorts, Config, HealthProbe, LinkState, LinkView, NoIpc, crashloop::BootMode,
};
use fogar_ports::gateway::GatewayLink;
use fogar_testkit::{
    FakeGatewayLink, FakeIpc, FakeMic, FakeSpeaker, FakeSystemProbe, MemorySecretStore, MockConfig,
    SeededRandom, TestClock, silence_chunks, tone_chunks,
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
}

pub(crate) fn test_config() -> Config {
    let mut c = Config {
        gateway: fogar_ports::gateway::GatewayAddr::parse("127.0.0.1").ok(),
        ..Config::default()
    };
    c.vad.hangover_chunks = 10;
    c
}

pub(crate) struct RigBuilder<L: GatewayLink> {
    pub(crate) cfg: Config,
    pub(crate) link: L,
    pub(crate) mic: FakeMic,
    pub(crate) speaker: FakeSpeaker,
    pub(crate) secrets: Arc<MemorySecretStore>,
    pub(crate) with_ipc: bool,
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
            secrets: Arc::new(MemorySecretStore::with_client_key(key)),
            with_ipc: false,
        }
    }

    pub(crate) fn start(self) -> Rig<L> {
        let clock = TestClock::new();
        let ipc = self.with_ipc.then(FakeIpc::new);
        let ports = AgentPorts {
            clock: Arc::new(clock),
            link: self.link.clone(),
            ipc: ipc.clone(),
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
        }
    }
}

/// `start_no_ipc` exists so tests without IPC do not need a FakeIpc type parameter.
pub(crate) fn start_no_ipc<L: GatewayLink + Clone>(
    b: RigBuilder<L>,
) -> (Agent<TestClock>, HealthProbe) {
    let clock = TestClock::new();
    let ports: AgentPorts<TestClock, L, NoIpc> = AgentPorts {
        clock: Arc::new(clock),
        link: b.link.clone(),
        ipc: None,
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
