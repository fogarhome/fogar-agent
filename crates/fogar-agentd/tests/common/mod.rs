//! Shared by the agent E2Es: a signed-out agent on the real adapters (`HttpGateway` for the
//! account and the `/events` link) with an in-memory, signed `FakeIpc` app side.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use fogar_core::{Agent, AgentPorts, Config, HealthProbe, LinkState, crashloop::BootMode};
use fogar_host::{HttpConfig, HttpGateway, TokioClock, WsConfig, WsGatewayLink};
use fogar_ports::{
    ipc::{GatewayInfo, IpcRequest, IpcResponse, LinkPhase, LoginForm, Password},
    secrets::{SecretCaps, SecretError, SecretKey, SecretStore, SecretString},
};
use fogar_testkit::{
    FakeIpc, FakeIpcClient, FakeMic, FakeSpeaker, MemorySecretStore, SeededRandom,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

/// Memory store that counts deletions of the account.
#[derive(Debug, Default)]
pub(crate) struct CountingStore {
    pub(crate) inner: MemorySecretStore,
    pub(crate) account_deletes: AtomicU32,
}

impl SecretStore for CountingStore {
    fn capabilities(&self) -> SecretCaps {
        self.inner.capabilities()
    }
    fn get(&self, key: SecretKey) -> Result<Option<SecretString>, SecretError> {
        self.inner.get(key)
    }
    fn put(&self, key: SecretKey, value: SecretString) -> Result<(), SecretError> {
        self.inner.put(key, value)
    }
    fn delete(&self, key: SecretKey) -> Result<(), SecretError> {
        if key == SecretKey::GatewayAccount && self.inner.contains(key) {
            self.account_deletes.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.delete(key)
    }
}

pub(crate) fn fast_config() -> Config {
    let mut c = Config::default();
    c.backoff.base = Duration::from_millis(20);
    c.backoff.cap = Duration::from_millis(200);
    c.backoff.floor = Duration::from_millis(5);
    // The mock and the real gateway listen on 127.0.0.1 (the dev-login-over-loopback switch).
    c.login_over_loopback = true;
    c
}

/// The gateway's `201` for `POST /auth/login`, handing out `token`.
pub(crate) fn login_ok(token: &str) -> String {
    format!(
        r#"{{"client_id":"c-1","token":"{token}","scopes":["conversations:read","conversations:write"],"user":{{"id":"u-1","username":"sito","display_name":"Sito","role":"member"}},"home":{{"id":"home","name":"mock"}}}}"#
    )
}

pub(crate) async fn within<F: std::future::Future>(d: Duration, f: F) -> F::Output {
    tokio::time::timeout(d, f).await.expect("timed out")
}

pub(crate) async fn ready(h: &mut HealthProbe, session: u64) {
    within(
        Duration::from_secs(10),
        h.wait_link(|v| v.state == LinkState::Ready { session }),
    )
    .await;
}

pub(crate) async fn login(
    c: &mut FakeIpcClient,
    address: &str,
    user: &str,
    password: &str,
) -> GatewayInfo {
    let r = c
        .request(IpcRequest::Login(Box::new(LoginForm {
            address: address.into(),
            username: user.into(),
            password: Password::parse(password),
        })))
        .await;
    match r {
        Some(IpcResponse::LoggedIn(info)) => *info,
        other => panic!("{other:?}"),
    }
}

pub(crate) async fn logout(c: &mut FakeIpcClient) -> bool {
    match c.request(IpcRequest::Logout).await {
        Some(IpcResponse::LoggedOut { gateway_reached }) => gateway_reached,
        other => panic!("{other:?}"),
    }
}

pub(crate) async fn phase(c: &mut FakeIpcClient) -> LinkPhase {
    match c.request(IpcRequest::Status).await {
        Some(IpcResponse::Status(s)) => s.phase,
        other => panic!("expected Status, got {other:?}"),
    }
}

pub(crate) fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

/// A signed-out agent on the real adapters, with a counting store and a signed fake IPC.
pub(crate) fn start_agent() -> (Agent<TokioClock>, Arc<CountingStore>, FakeIpc) {
    let secrets = Arc::new(CountingStore::default());
    let ipc = FakeIpc::signed();
    let ports = AgentPorts {
        clock: Arc::new(TokioClock::new()),
        link: WsGatewayLink::new(WsConfig::default()),
        ipc: Some(ipc.clone()),
        http: HttpGateway::new(HttpConfig::default()),
        tailnet: fogar_core::NoTailnet,
        secrets: secrets.clone(),
        random: Arc::new(SeededRandom::new(11)),
        audio_in: Arc::new(FakeMic::minimal()),
        audio_out: Arc::new(FakeSpeaker::minimal()),
        probe: None,
    };
    let agent = Agent::start(ports, fast_config(), BootMode::Normal);
    (agent, secrets, ipc)
}

/// What the app does with its conversation window open: one connection subscribed to the
/// conversation and the list. Answers come back in order; pushes are kept as the latest of each.
pub(crate) struct App {
    pub(crate) c: FakeIpcClient,
    pub(crate) view: fogar_ports::ipc::ConversationView,
    pub(crate) list: Vec<fogar_ports::ipc::ConversationSummary>,
}

impl App {
    pub(crate) async fn open(ipc: &FakeIpc) -> Self {
        let mut app = Self {
            c: ipc.client().await,
            view: Default::default(),
            list: Vec::new(),
        };
        assert_eq!(
            app.ask(IpcRequest::SubscribeConversation).await,
            IpcResponse::ConversationSubscribed
        );
        assert_eq!(
            app.ask(IpcRequest::ListConversations).await,
            IpcResponse::ListSubscribed
        );
        app
    }

    fn take(&mut self, r: IpcResponse) -> Option<IpcResponse> {
        match r {
            IpcResponse::Conversation(v) => self.view = v,
            IpcResponse::Conversations(l) => self.list = l,
            IpcResponse::Event(_) => {}
            other => return Some(other),
        }
        None
    }

    /// The answer to `r` (pushes before it are taken in).
    pub(crate) async fn ask(&mut self, r: IpcRequest) -> IpcResponse {
        let mut next = within(Duration::from_secs(10), self.c.request(r)).await;
        loop {
            match next.map(|m| self.take(m)) {
                Some(Some(answer)) => return answer,
                Some(None) => next = within(Duration::from_secs(10), self.c.next()).await,
                None => panic!("the agent closed the connection"),
            }
        }
    }

    /// Reads pushes until `ok` holds for the latest view and list.
    pub(crate) async fn until(
        &mut self,
        what: &str,
        d: Duration,
        mut ok: impl FnMut(&Self) -> bool,
    ) {
        let end = tokio::time::Instant::now() + d;
        while !ok(self) {
            match tokio::time::timeout_at(end, self.c.next()).await {
                Ok(Some(m)) => {
                    if let Some(other) = self.take(m) {
                        panic!("unasked answer {other:?}");
                    }
                }
                Ok(None) => panic!("the agent closed the connection"),
                Err(_) => panic!("never: {what} (view {:?}, list {:?})", self.view, self.list),
            }
        }
    }
}
