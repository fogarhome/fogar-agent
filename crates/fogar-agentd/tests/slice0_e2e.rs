//! S4 PR 4a end-to-end: the chat over the REAL transport (`fogar_host::HttpGateway`: the login,
//! `GET /events` and the conversation REST calls) against the Rust mock (`MockGateway` +
//! `EventsHub`, one loopback port), driven the way the app drives it over IPC v6 (the in-memory
//! signed `FakeIpc`: the real socket's code-signing check cannot accept a test binary).
//! login -> the list -> send (creates a conversation) -> the reply -> another device writes ->
//! a slow reply is stopped. The hub rotates its stream every 2 s, so all of it also crosses
//! rotations. Real sockets, so real time.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::*;
use fogar_ports::ipc::{
    ConversationView, IpcRequest, IpcResponse, MessageState, TurnOutcome, UserText, ViewEntry,
};
use fogar_testkit::{EventsHub, MockConfig, MockGateway};
use std::time::Duration;

fn text(t: &str) -> IpcRequest {
    IpcRequest::SendText {
        text: UserText(t.into()),
    }
}

fn last_turn(v: &ConversationView) -> Option<(&str, Option<TurnOutcome>)> {
    v.entries.iter().rev().find_map(|e| match e {
        ViewEntry::Turn(t) => Some((t.reply.as_str(), t.outcome)),
        _ => None,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_over_events_against_the_mock_gateway() {
    let gw = MockGateway::start(MockConfig::default()).await.unwrap();
    let hub = EventsHub::new();
    hub.install(&gw.routes());
    hub.set_config(|c| c.stream_for = Duration::from_secs(2));
    hub.seed_conversation("c-1", "La serie");
    hub.seed_history("c-1", "user", "pon la serie");
    hub.seed_history("c-1", "assistant", "puesta");
    gw.routes()
        .set("POST", "/auth/login", 201, login_ok("key-1"));
    let baseline = alive_tasks();
    // Streams live 2 s here: only one older than `stable_stream` is a rotation (invisible),
    // a younger one is a real `Down` that would start a new session.
    let mut cfg = fast_config();
    cfg.stable_stream = Duration::from_secs(1);
    let (agent, _secrets, ipc) = start_agent_with(cfg);
    let mut h = agent.health();
    let mut app = ipc.client().await;
    login(&mut app, &gw.addr().to_string(), "sito", "contraseña-1").await;
    ready(&mut h, 1).await;

    // The list arrives on its own; nothing is open.
    let mut chat = App::open(&ipc).await;
    chat.until("the list", Duration::from_secs(10), |a| {
        a.list
            .iter()
            .any(|c| c.id == "c-1" && c.title.as_deref() == Some("La serie"))
    })
    .await;
    assert_eq!(chat.view.id, None);

    // Nothing open: the text creates a conversation; my message and the reply come by `/events`.
    assert_eq!(chat.ask(text("hola")).await, IpcResponse::TextAccepted);
    chat.until("the reply", Duration::from_secs(15), |a| {
        a.view.entries.iter().any(|e| {
            matches!(e, ViewEntry::Message(m) if m.mine && m.text == "hola" && m.state == MessageState::Delivered)
        }) && last_turn(&a.view) == Some(("Hola mundo", Some(TurnOutcome::Done)))
    })
    .await;
    let new = chat.view.id.clone().unwrap();
    chat.until("new one on top", Duration::from_secs(10), |a| {
        a.list.first().is_some_and(|c| c.id == new) && a.list.len() == 2
    })
    .await;

    // Opening the seeded one shows its history.
    assert_eq!(
        chat.ask(IpcRequest::OpenConversation { id: "c-1".into() })
            .await,
        IpcResponse::ConversationOpened
    );
    chat.until("its history", Duration::from_secs(10), |a| {
        a.view.id.as_deref() == Some("c-1")
            && a.view.entries.iter().any(|e| {
                matches!(e, ViewEntry::Message(m) if m.text == "pon la serie" && m.state == MessageState::Past)
            })
    })
    .await;

    // Another device writes: shown with its name, and the list marks it «en marcha».
    hub.set_reply(&["a", "b", "c", "d"], Duration::from_millis(700));
    hub.other_device_send("c-1", "¿y el tiempo?");
    chat.until("from another device, running", Duration::from_secs(10), |a| {
        a.view.entries.iter().any(|e| {
            matches!(e, ViewEntry::Message(m) if !m.mine && m.text == "¿y el tiempo?" && m.from_name.is_some())
        }) && a.list.first().is_some_and(|c| c.id == "c-1" && c.running)
    })
    .await;

    // Stop: the turn (started elsewhere) ends cancelled and the mark goes.
    chat.until("its turn runs", Duration::from_secs(10), |a| {
        last_turn(&a.view).is_some_and(|(_, o)| o.is_none())
    })
    .await;
    assert_eq!(
        chat.ask(IpcRequest::CancelTurn).await,
        IpcResponse::TurnCancelSent
    );
    chat.until("cancelled", Duration::from_secs(10), |a| {
        last_turn(&a.view).is_some_and(|(_, o)| o == Some(TurnOutcome::Cancelled))
            && a.list.iter().all(|c| !c.running)
    })
    .await;
    // Past two rotations, a text still gets its whole reply (spread over another rotation),
    // and the link stayed up all along (the first session).
    let opens = hub.stream_opens();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(hub.stream_opens() >= opens + 2, "the stream rotated");
    assert_eq!(chat.ask(text("otra")).await, IpcResponse::TextAccepted);
    chat.until("a reply across a rotation", Duration::from_secs(15), |a| {
        last_turn(&a.view) == Some(("abcd", Some(TurnOutcome::Done)))
    })
    .await;
    let snap = h.snapshot();
    assert_eq!(
        snap.link,
        fogar_core::LinkState::Ready { session: 1 },
        "rotations, never a new session"
    );
    assert!(snap.stats.stream_rotations >= 2, "{:?}", snap.stats);

    drop(chat);
    drop(app);
    assert!(agent.shutdown(Duration::from_secs(5)).await.clean);
    gw.shutdown().await;
    drop(hub);
    within(Duration::from_secs(5), async {
        while alive_tasks() > baseline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
}
