//! Engine acceptance (S4 PR 4a Task 4), paused time over the `/events` hub. In-crate: these
//! drive engine commands directly (the IPC side is `tests/conversation.rs`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]

use super::*;
use crate::{
    Agent, HealthProbe, account,
    capture::CaptureGate,
    conversation::{Entry, MsgStatus},
    link::Api,
    queues::{self, ByteBudget},
    stats::LinkState,
};
use fogar_ports::{
    gateway::GatewayAddr,
    ipc::{CaptureGesture, MessageState, TurnOutcome, ViewEntry},
    secrets::{SecretKey, SecretStore},
};
use fogar_proto::events::decode_event;
use fogar_testkit::{
    EventsHub, FakeGatewayHttp, FakeMic, FakeSpeaker, HttpRoutes, MemorySecretStore, Scope,
    TestClock, gateway::SeenRequest,
};
use std::convert::Infallible;
use std::time::Duration;

const GW: &str = "127.0.0.1:8004";

struct Rig {
    agent: Agent<TestClock>,
    health: HealthProbe,
    hub: EventsHub,
    routes: HttpRoutes,
    mic: FakeMic,
    speaker: FakeSpeaker,
}

fn rig_with(setup: impl FnOnce(&EventsHub, &HttpRoutes)) -> Rig {
    let routes = HttpRoutes::fogar();
    let hub = EventsHub::new();
    hub.install(&routes);
    setup(&hub, &routes);
    let secrets = MemorySecretStore::default();
    secrets
        .put(SecretKey::GatewayClientKey, "k".to_owned().into())
        .unwrap();
    account::adopt_stored_token(&secrets, GatewayAddr::parse(GW).unwrap()).unwrap();
    let mic = FakeMic::healthy();
    let speaker = FakeSpeaker::healthy();
    let agent = crate::agent::start_for_test(
        FakeGatewayHttp::at(routes.clone(), GatewayAddr::parse(GW).unwrap()),
        Arc::new(secrets),
        mic.clone(),
        speaker.clone(),
    );
    Rig {
        health: agent.health(),
        agent,
        hub,
        routes,
        mic,
        speaker,
    }
}

fn rig() -> Rig {
    rig_with(|hub, _| {
        hub.seed_conversation("c-1", "Serie");
        hub.seed_history("c-1", "user", "pon la serie");
        hub.seed_history("c-1", "assistant", "puesta");
    })
}

async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let r = tokio::time::timeout(Duration::from_secs(60), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(r.is_ok(), "never: {what}");
}

impl Rig {
    async fn ready(&mut self) {
        let mut h = self.health.clone();
        tokio::time::timeout(
            Duration::from_secs(60),
            h.wait_link(|v| matches!(v.state, LinkState::Ready { .. })),
        )
        .await
        .unwrap();
        // The fresh link's list (and `/auth/me`) settle.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    async fn cmd<R>(&self, make: impl FnOnce(oneshot::Sender<R>) -> Command) -> Option<R> {
        let (tx, rx) = oneshot::channel();
        self.agent.cmds.send(make(tx)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(30), rx)
            .await
            .unwrap()
            .ok()
    }
    async fn open(&self, id: &str) {
        self.cmd(|tx| Command::Open(id.into(), tx)).await.unwrap();
        let id = id.to_owned();
        let h = self.health.clone();
        eventually("open", || {
            h.chat()
                .open
                .as_ref()
                .is_some_and(|o| o.id == id && o.is_live())
        })
        .await;
    }
    /// Answered once the call is admitted; returns after it ran.
    async fn send(&self, text: &str) -> Option<Result<(), TextRefusal>> {
        let r = self.cmd(|tx| Command::SendText(text.into(), tx)).await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        r
    }
    async fn cancel(&self) -> Option<bool> {
        let r = self.cmd(Command::Cancel).await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        r
    }
    fn entries(&self) -> Vec<Entry> {
        self.health
            .chat()
            .open
            .map(|o| o.entries)
            .unwrap_or_default()
    }
    /// `(message, reply)` pairs of the view.
    fn shown(&self) -> Vec<(String, String)> {
        crate::conversation::pairs(&self.health.conversation())
    }
    fn last_turn(&self) -> Option<fogar_ports::ipc::TurnView> {
        crate::conversation::last_turn(&self.health.conversation()).cloned()
    }
    fn calls(&self, method: &str, prefix: &str) -> Vec<SeenRequest> {
        self.routes
            .seen()
            .into_iter()
            .filter(|r| r.method == method && r.path.starts_with(prefix))
            .collect()
    }
    async fn stop(self) {
        assert!(self.agent.shutdown(Duration::from_secs(4)).await.clean);
    }
}

fn pair(m: &str, r: &str) -> (String, String) {
    (m.to_owned(), r.to_owned())
}

#[tokio::test(start_paused = true)]
async fn list_open_and_create_without_agent() {
    let mut r = rig();
    r.ready().await;
    let chat = r.health.chat();
    assert_eq!(chat.list.len(), 1);
    assert_eq!(
        (chat.list[0].id.as_str(), chat.list[0].title.as_deref()),
        ("c-1", Some("Serie"))
    );
    assert_eq!(r.calls("GET", "/auth/me").len(), 1, "once per account");
    // Nothing open: the text creates a conversation (no agent: the gateway's default).
    assert_eq!(r.send("hola").await, Some(Ok(())));
    let creates = r.calls("POST", "/conversations");
    assert_eq!(creates.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&creates[0].body).unwrap();
    assert_eq!(body, serde_json::json!({"message": "hola"}));
    eventually("reply", || r.shown() == [pair("hola", "Hola mundo")]).await;
    let chat = r.health.chat();
    assert_eq!(chat.list[0].id, chat.open_id().unwrap(), "new one on top");
    assert_eq!(chat.list.len(), 2);
    // Opening another shows its history.
    r.open("c-1").await;
    assert_eq!(r.shown(), [pair("pon la serie", "puesta")]);
    assert_eq!(
        r.calls("GET", "/conversations/c-1/messages")[0].path,
        "/conversations/c-1/messages?limit=50"
    );
    // The next text goes to the open one.
    assert_eq!(r.send("otra").await, Some(Ok(())));
    assert_eq!(r.calls("POST", "/conversations/c-1/messages").len(), 1);
    eventually("second", || {
        r.shown() == [pair("pon la serie", "puesta"), pair("otra", "Hola mundo")]
    })
    .await;
    r.stop().await;
}

/// «Nueva»: nothing open, and a late state reply of the conversation it left never brings
/// that one back.
#[tokio::test(start_paused = true)]
async fn new_conversation_ignores_a_late_open() {
    let mut r = rig_with(|hub, routes| {
        hub.seed_conversation("c-1", "Serie");
        routes.set_delayed(
            "GET",
            "/conversations/c-1",
            200,
            r#"{"id":"c-1","agent":"main","created_at":"x","turn":null,"pending":[],"as_of":0}"#,
            Duration::from_secs(3),
        );
    });
    r.ready().await;
    r.cmd(|tx| Command::Open("c-1".into(), tx)).await.unwrap();
    eventually("opening", || r.health.conversation().id.is_some()).await;
    r.cmd(Command::New).await.unwrap();
    tokio::time::sleep(Duration::from_secs(30)).await;
    let v = r.health.conversation();
    assert_eq!((v.id, v.entries.len()), (None, 0));
    assert!(r.health.chat().open.is_none());
    r.stop().await;
}

/// A create that fails with nothing open is shown (failed), and «Nueva» clears it.
#[tokio::test(start_paused = true)]
async fn failed_create_is_shown() {
    let mut r = rig_with(|hub, routes| {
        hub.seed_conversation("c-1", "Serie");
        routes.set("POST", "/conversations", 500, "{}");
    });
    r.ready().await;
    assert_eq!(r.send("hola").await, Some(Ok(())));
    eventually("failed shown", || {
        matches!(r.health.conversation().entries.as_slice(),
            [ViewEntry::Message(m)] if m.text == "hola" && m.mine && m.state == MessageState::Failed)
    })
    .await;
    assert_eq!(r.health.conversation().id, None);
    r.cmd(Command::New).await.unwrap();
    eventually("cleared", || r.health.conversation().entries.is_empty()).await;
    r.stop().await;
}

#[tokio::test(start_paused = true)]
async fn send_with_a_turn_running_is_pending_then_delivered() {
    let mut r = rig();
    r.hub.set_reply(&["a", "b", "c"], Duration::from_secs(2));
    r.ready().await;
    r.open("c-1").await;
    r.send("uno").await.unwrap().unwrap();
    eventually("turn runs", || r.health.chat().list[0].running).await;
    r.send("dos").await.unwrap().unwrap();
    let status = |r: &Rig| {
        r.entries().iter().rev().find_map(|e| match e {
            Entry::Msg(m) if m.text == "dos" => Some(m.status),
            _ => None,
        })
    };
    eventually("pending", || status(&r) == Some(MsgStatus::Pending)).await;
    eventually("delivered", || status(&r) == Some(MsgStatus::Delivered)).await;
    eventually("both replied", || {
        r.shown()[1..] == [pair("uno", "abc"), pair("dos", "abc")]
    })
    .await;
    assert!(!r.health.chat().list[0].running);
    r.stop().await;
}

#[tokio::test(start_paused = true)]
async fn other_device_message_shows_its_name() {
    let mut r = rig();
    r.ready().await;
    r.open("c-1").await;
    r.hub.other_device_send("c-1", "desde la cocina");
    eventually("shown", || r.shown().len() == 2).await;
    let Entry::Msg(m) = &r.entries()[2] else {
        panic!("{:?}", r.entries())
    };
    assert_eq!((m.mine, m.from_name.as_deref()), (false, Some("Cocina")));
    r.stop().await;
}

/// A `resync` (the gateway lost our place) fetches the list and the open conversation again; a
/// plain rotation fetches nothing.
#[tokio::test(start_paused = true)]
async fn resync_refetches() {
    let mut r = rig();
    r.ready().await;
    r.open("c-1").await;
    tokio::time::sleep(Duration::from_secs(60)).await; // rotations
    let before = (
        r.calls("GET", "/conversations").len(),
        r.calls("GET", "/auth/me").len(),
    );
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(
        (
            r.calls("GET", "/conversations").len(),
            r.calls("GET", "/auth/me").len()
        ),
        before,
        "rotations fetch nothing"
    );
    // A whole turn the link misses, then the gateway forgets our place.
    r.hub.go_silent();
    r.hub.other_device_send("c-1", "perdido");
    tokio::time::sleep(Duration::from_secs(5)).await;
    r.hub.forget_buffer();
    eventually("resync", || r.health.snapshot().stats.events_resync == 1).await;
    eventually("refetched", || {
        r.calls("GET", "/conversations").len() >= before.0 + 3
            && r.health.chat().open.unwrap().is_live()
    })
    .await;
    assert_eq!(
        r.calls("GET", "/auth/me").len(),
        before.1,
        "not per attempt"
    );
    assert_eq!(r.shown()[1], pair("perdido", "Hola mundo"), "from history");
    r.stop().await;
}

/// Review focus 1: a conversation opened while its turn streams shows the message and the whole
/// reply once.
///
/// Known gap (GUESSED, gateway side): opened between a message going `delivered` and its turn
/// starting (the hub's 200 ms `first_token_after`), the message is in neither the state's
/// `pending` nor the history, so only its reply shows. Not covered here; reported.
#[tokio::test(start_paused = true)]
async fn open_during_live_events_loses_nothing() {
    for wait_ms in [250, 700, 1_300, 2_100, 4_000] {
        let mut r = rig();
        r.hub
            .set_reply(&["uno ", "dos ", "tres"], Duration::from_millis(500));
        r.ready().await;
        r.hub.other_device_send("c-1", "cuenta");
        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        r.open("c-1").await;
        eventually("done", || {
            r.last_turn().unwrap().outcome == Some(TurnOutcome::Done) && r.shown().len() == 2
        })
        .await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(
            r.shown(),
            [
                pair("pon la serie", "puesta"),
                pair("cuenta", "uno dos tres")
            ],
            "opened after {wait_ms} ms"
        );
        r.stop().await;
    }
}

#[tokio::test(start_paused = true)]
async fn stop_a_turn_started_elsewhere() {
    let mut r = rig();
    r.hub.set_reply(&["a", "b", "c"], Duration::from_secs(5));
    r.ready().await;
    r.open("c-1").await;
    r.hub.other_device_send("c-1", "larga");
    eventually("running", || {
        r.last_turn().unwrap().turn_id.starts_with("turn")
    })
    .await;
    assert_eq!(r.cancel().await, Some(true));
    assert_eq!(r.calls("POST", "/conversations/c-1/cancel").len(), 1);
    eventually("cancelled", || {
        r.last_turn().unwrap().outcome == Some(TurnOutcome::Cancelled)
    })
    .await;
    assert!(!r.health.chat().list[0].running);
    r.stop().await;
}

/// Sito: a withdrawn message disappears (here: «stop» before its turn started withdraws it).
#[tokio::test(start_paused = true)]
async fn withdrawn_message_disappears() {
    let mut r = rig();
    r.ready().await;
    r.open("c-1").await;
    r.send("me arrepiento").await.unwrap().unwrap();
    eventually("shown", || r.shown().len() == 2).await;
    r.cancel().await.unwrap();
    eventually("gone", || r.shown().len() == 1).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(r.shown(), [pair("pon la serie", "puesta")]);
    r.stop().await;
}

#[tokio::test(start_paused = true)]
async fn list_marks_running_and_moves_to_top() {
    let mut r = rig_with(|hub, _| {
        hub.seed_conversation("c-1", "uno");
        hub.seed_conversation("c-2", "dos");
    });
    r.hub.set_reply(&["a"], Duration::from_secs(3));
    r.ready().await;
    let ids = |r: &Rig| -> Vec<(String, bool)> {
        r.health
            .chat()
            .list
            .into_iter()
            .map(|c| (c.id, c.running))
            .collect()
    };
    assert_eq!(ids(&r), [("c-2".into(), false), ("c-1".into(), false)]);
    r.hub.other_device_send("c-1", "hola");
    eventually("running on top", || {
        ids(&r) == [("c-1".into(), true), ("c-2".into(), false)]
    })
    .await;
    // The UI's list (its own watch) says the same.
    let ui: Vec<(String, bool)> = r
        .health
        .list
        .borrow()
        .iter()
        .map(|c| (c.id.clone(), c.running))
        .collect();
    assert_eq!(ui, ids(&r));
    eventually("ended", || ids(&r)[0] == ("c-1".into(), false)).await;
    // A conversation created elsewhere: the list is fetched once (debounced).
    let lists = r.calls("GET", "/conversations").len();
    let c3 = {
        let c = fogar_ports::gateway_http::HttpCall::CreateConversation;
        let http = FakeGatewayHttp::at(r.routes.clone(), GatewayAddr::parse(GW).unwrap());
        let body = br#"{"message":"desde otro"}"#.to_vec();
        let rep = http
            .request(
                &GatewayAddr::parse(GW).unwrap(),
                &c,
                Some(&"k".to_owned().into()),
                Some(body.into()),
            )
            .await
            .unwrap();
        serde_json::from_slice::<serde_json::Value>(&rep.body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    eventually("listed", || ids(&r).iter().any(|(id, _)| *id == c3)).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(r.calls("GET", "/conversations").len(), lists + 1);
    r.stop().await;
}

#[tokio::test(start_paused = true)]
async fn view_stays_within_48_kib() {
    let mut r = rig();
    let big = "é".repeat(4000);
    r.hub.set_reply(&[&big, &big, &big], Duration::ZERO);
    r.ready().await;
    r.open("c-1").await;
    for i in 0..30 {
        r.hub.other_device_send("c-1", &format!("vez {i}"));
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let v = r.health.conversation();
    assert!(v.entries.len() <= crate::conversation::MAX_ENTRIES);
    let shown = r.shown();
    let text: usize = shown.iter().map(|(m, t)| m.len() + t.len()).sum();
    assert!(
        text <= fogar_ports::ipc::CONVERSATION_MAX_TEXT_BYTES,
        "{text}"
    );
    assert_eq!(shown.last().unwrap().0, "vez 29");
    r.stop().await;
}

/// Review focus 6: push audio never reaches the speaker; the push text shows.
#[tokio::test(start_paused = true)]
async fn push_audio_never_plays() {
    let mut r = rig();
    r.hub.set_config(|c| c.audio_to_all = true);
    r.ready().await;
    r.open("c-1").await;
    r.hub.push("c-1", "recordatorio", true);
    eventually("shown", || r.shown().len() == 2).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(r.shown()[1], pair("", "recordatorio"));
    assert_eq!(r.speaker.snapshot().opens, 0);
    assert_eq!(r.health.snapshot().stats.events_audio_dropped, 1);
    r.stop().await;
}

/// PR 3 carry-over: `403 missing_scope` on a write is posted once, shown as failed, and every
/// later write is refused without a call.
#[tokio::test(start_paused = true)]
async fn missing_scope_on_send_is_refused_once() {
    let mut r = rig_with(|hub, _| {
        hub.seed_conversation("c-1", "Serie");
        hub.missing_scope(Scope::Write);
    });
    r.ready().await;
    r.open("c-1").await;
    assert_eq!(r.send("hola").await, Some(Ok(())));
    eventually("failed", || {
        r.health.conversation().entries.last().is_some_and(|e| {
            matches!(e, ViewEntry::Message(m) if m.state == MessageState::Failed
                && m.error_code.as_deref() == Some("missing_scope"))
        })
    })
    .await;
    assert!(r.health.conversation().read_only);
    assert_eq!(r.send("otra").await, Some(Err(TextRefusal::MissingScope)));
    assert_eq!(r.cancel().await, Some(true));
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(r.calls("POST", "/conversations").len(), 1, "never retried");
    let s = r.health.snapshot().stats;
    assert_eq!(s.writes_missing_scope, 1);
    r.stop().await;
}

/// PR 3 carry-over: `/auth/me` without `conversations:write` refuses writes before any call.
#[tokio::test(start_paused = true)]
async fn auth_me_without_write_scope_refuses_sends() {
    let mut r = rig_with(|hub, routes| {
        hub.seed_conversation("c-1", "Serie");
        routes.set(
            "GET",
            "/auth/me",
            200,
            r#"{"user":{"id":"u","username":"s","display_name":"S","role":"member"},"scopes":["conversations:read"]}"#,
        );
    });
    r.ready().await;
    assert!(r.health.conversation().read_only);
    assert_eq!(r.send("hola").await, Some(Err(TextRefusal::MissingScope)));
    assert!(r.calls("POST", "/conversations").is_empty());
    r.stop().await;
}

/// Review L5: a command the IPC abandoned (its reply dropped, as after an engine stall) is
/// never run late.
#[tokio::test(start_paused = true)]
async fn abandoned_command_is_not_run_late() {
    let mut r = rig();
    r.ready().await;
    r.open("c-1").await;
    let (tx, rx) = oneshot::channel();
    drop(rx);
    r.agent
        .cmds
        .send(Command::SendText("tarde".into(), tx))
        .await
        .unwrap();
    let (tx, rx) = oneshot::channel();
    drop(rx);
    r.agent.cmds.send(Command::Cancel(tx)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(r.calls("POST", "/conversations").is_empty());
    r.stop().await;
}

/// Dictation is off until PR 5: with the link up a take is refused (no transcriber) and the
/// microphone never opens.
#[tokio::test(start_paused = true)]
async fn dictation_is_refused_until_pr5() {
    let mut r = rig();
    r.ready().await;
    let (_owner, owner) = oneshot::channel::<Infallible>();
    let start = Start::UserAct(crate::ipc::UserAct::for_test(CaptureGesture::Toggle));
    let got = r
        .cmd(|reply| Command::CaptureStart {
            start,
            owner,
            reply,
        })
        .await;
    assert_eq!(got, Some(Err(CaptureRefusal::NoTranscriber)));
    assert_eq!(r.mic.opens(), 0);
    r.stop().await;
}

// ---- Task 4 review ----------------------------------------------------------------------

/// Texts of the open conversation, messages and replies in order.
fn order(r: &Rig) -> Vec<String> {
    r.entries()
        .into_iter()
        .map(|e| match e {
            Entry::Msg(m) => m.text,
            Entry::Turn(t) => t.reply,
        })
        .collect()
}

/// A failed (or unknown) entry of mine, with its code.
fn failed(r: &Rig) -> Vec<(String, Option<String>)> {
    r.entries()
        .into_iter()
        .filter_map(|e| match e {
            Entry::Msg(m) if m.status == MsgStatus::Failed => Some((m.text, m.error_code)),
            _ => None,
        })
        .collect()
}

/// Review H1: two texts typed during a reply; each reply shows right after its own message.
#[tokio::test(start_paused = true)]
async fn texts_typed_during_a_reply_each_get_theirs_next() {
    let mut r = rig();
    r.hub.set_reply(&["a", "b", "c"], Duration::from_secs(1));
    r.ready().await;
    r.open("c-1").await;
    r.send("uno").await.unwrap().unwrap();
    eventually("running", || r.health.chat().list[0].running).await;
    r.send("dos").await.unwrap().unwrap();
    r.send("tres").await.unwrap().unwrap();
    eventually("three replies", || {
        order(&r)[2..] == ["uno", "abc", "dos", "abc", "tres", "abc"]
    })
    .await;
    r.stop().await;
}

/// Review M1: a `turn_end` the link never saw (the gateway forgot our place) leaves no
/// «running» mark after the resync.
#[tokio::test(start_paused = true)]
async fn resync_forgets_running_marks() {
    let mut r = rig_with(|hub, _| {
        hub.seed_conversation("c-1", "uno");
        hub.seed_conversation("c-2", "dos");
    });
    r.hub.set_reply(&["a", "b", "c"], Duration::from_secs(3));
    r.ready().await;
    r.hub.other_device_send("c-2", "larga");
    eventually("running", || r.health.chat().list[0].running).await;
    r.hub.go_silent();
    tokio::time::sleep(Duration::from_secs(15)).await; // the turn ends unseen
    r.hub.forget_buffer();
    eventually("resync", || r.health.snapshot().stats.events_resync == 1).await;
    eventually("no stale mark", || {
        r.health.chat().list.iter().all(|c| !c.running)
    })
    .await;
    r.stop().await;
}

/// Review M2a: a post with no answer within our deadline may have arrived: «unknown», not
/// «not sent».
#[tokio::test(start_paused = true)]
async fn a_post_past_the_deadline_is_unknown() {
    let mut r = rig();
    r.routes.set_delayed(
        "POST",
        "/conversations/c-1/messages",
        202,
        r#"{"message_id":"run-x"}"#,
        Duration::from_secs(60),
    );
    r.ready().await;
    r.open("c-1").await;
    r.send("hola").await.unwrap().unwrap();
    eventually("failed", || !failed(&r).is_empty()).await;
    assert_eq!(failed(&r), [("hola".into(), Some("unknown".into()))]);
    r.stop().await;
}

/// Review M2b: a failure shows only in the conversation it was posted to.
#[tokio::test(start_paused = true)]
async fn a_failed_post_shows_only_in_its_conversation() {
    let mut r = rig_with(|hub, routes| {
        hub.seed_conversation("c-1", "uno");
        hub.seed_conversation("c-2", "dos");
        // Fails only after the other conversation is open (a real race).
        routes.set_delayed(
            "POST",
            "/conversations/c-1/messages",
            500,
            "{}",
            Duration::from_secs(1),
        );
    });
    r.ready().await;
    r.open("c-1").await;
    r.cmd(|tx| Command::SendText("hola".into(), tx))
        .await
        .unwrap()
        .unwrap();
    // Another one opened before the answer is handled.
    r.cmd(|tx| Command::Open("c-2".into(), tx)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(r.health.chat().open_id(), Some("c-2"));
    assert!(failed(&r).is_empty(), "{:?}", r.entries());
    assert!(r.health.snapshot().stats.rest_failures >= 1);
    r.stop().await;
}

/// Review M2c and M4: a resync of the open conversation keeps a failed text, and what was
/// shown stays until the history replaces it.
#[tokio::test(start_paused = true)]
async fn a_resync_keeps_the_screen_and_a_failed_text() {
    let mut r = rig_with(|hub, routes| {
        hub.seed_conversation("c-1", "uno");
        hub.seed_conversation("c-2", "dos");
        hub.seed_history("c-1", "user", "pon la serie");
        hub.seed_history("c-1", "assistant", "puesta");
        routes.set("POST", "/conversations/c-1/messages", 500, "{}");
        // The re-open's history is slow: the screen must not go blank meanwhile.
    });
    r.ready().await;
    r.open("c-1").await;
    r.send("no salió").await.unwrap().unwrap();
    eventually("failed", || !failed(&r).is_empty()).await;
    r.routes.set_delayed(
        "GET",
        "/conversations/c-1/messages?limit=50",
        200,
        r#"{"conversation_id":"c-1","messages":[{"id":"h1","role":"user","text":"pon la serie"},{"id":"h2","role":"assistant","text":"puesta"}],"has_more":false}"#,
        Duration::from_secs(3),
    );
    r.hub.drop_streams();
    r.hub.other_device_send("c-2", "missed");
    r.hub.forget_buffer();
    eventually("resync", || r.health.snapshot().stats.events_resync == 1).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!r.health.chat().open.unwrap().is_live(), "still loading");
    assert_eq!(order(&r), ["pon la serie", "puesta", "no salió"], "kept");
    eventually("live", || r.health.chat().open.unwrap().is_live()).await;
    assert_eq!(order(&r), ["pon la serie", "puesta", "no salió"]);
    r.stop().await;
}

/// Review L5: an unlisted conversation that keeps streaming asks for the list at most once
/// per 30 s.
#[tokio::test(start_paused = true)]
async fn an_unlisted_conversation_refreshes_the_list_rarely() {
    let mut r = rig_with(|hub, routes| {
        hub.seed_conversation("c-9", "fuera");
        routes.set("GET", "/conversations", 200, "[]");
    });
    r.hub.set_reply(&["a"; 30], Duration::from_secs(3));
    r.ready().await;
    let before = r.calls("GET", "/conversations").len();
    r.hub.other_device_send("c-9", "habla");
    tokio::time::sleep(Duration::from_secs(91)).await;
    let n = r.calls("GET", "/conversations").len() - before;
    assert!((2..=4).contains(&n), "{n} list calls in 91 s");
    r.stop().await;
}

/// Review L9: a scope granted later (seen on `/auth/me` at a resync) lets texts through again.
#[tokio::test(start_paused = true)]
async fn a_granted_scope_is_seen_at_the_next_resync() {
    const READ_ONLY: &str = r#"{"user":{"id":"u","username":"s","display_name":"S","role":"member"},"scopes":["conversations:read"]}"#;
    const WRITE: &str = r#"{"user":{"id":"u","username":"s","display_name":"S","role":"member"},"scopes":["conversations:read","conversations:write"]}"#;
    let mut r = rig_with(|hub, routes| {
        hub.seed_conversation("c-1", "Serie");
        hub.seed_conversation("c-2", "otra");
        routes.set("GET", "/auth/me", 200, READ_ONLY);
    });
    r.ready().await;
    r.open("c-1").await;
    assert_eq!(r.send("hola").await, Some(Err(TextRefusal::MissingScope)));
    r.routes.set("GET", "/auth/me", 200, WRITE);
    r.hub.drop_streams();
    r.hub.other_device_send("c-2", "missed");
    r.hub.forget_buffer();
    eventually("resync", || r.health.snapshot().stats.events_resync == 1).await;
    eventually("me again", || r.calls("GET", "/auth/me").len() == 2).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(r.send("hola").await, Some(Ok(())));
    r.stop().await;
}

/// The engine over the hub's routes, fed link events by hand (to bump the account epoch).
struct Alone {
    in_tx: mpsc::Sender<Inbound>,
    cmd_tx: mpsc::Sender<Command>,
    chat: watch::Receiver<Chat>,
    stats: Arc<Stats>,
    stop: CancellationToken,
    tasks: JoinSet<()>,
}

fn alone(routes: &HttpRoutes) -> Alone {
    let cfg = Arc::new(Config::default());
    let stats = Arc::new(Stats::default());
    let (in_tx, in_rx) = mpsc::channel(queues::INBOUND);
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(queues::ENGINE_COMMANDS);
    let (conv_tx, _conv) = watch::channel(ConversationView::default());
    let (chat_tx, chat) = watch::channel(Chat::default());
    let (list_tx, _list) = watch::channel(Vec::new());
    let engine = Engine {
        clock: Arc::new(TestClock::new()),
        cfg: cfg.clone(),
        stats: stats.clone(),
        http: Arc::new(FakeGatewayHttp::at(
            routes.clone(),
            GatewayAddr::parse(GW).unwrap(),
        )),
        in_rx,
        cmd_rx,
        conv: conv_tx,
        chat: chat_tx,
        list: list_tx,
    };
    let capture = CaptureGate::new(Arc::new(FakeMic::healthy()), &cfg, stats.clone());
    let playback = PlaybackGate::new(Arc::new(FakeSpeaker::healthy()), stats.clone());
    let stop = CancellationToken::new();
    let mut tasks = JoinSet::new();
    tasks.spawn(engine.run(capture, playback, stop.clone()));
    Alone {
        in_tx,
        cmd_tx,
        chat,
        stats,
        stop,
        tasks,
    }
}

fn fresh_up() -> Inbound {
    Inbound::Up {
        api: Api {
            addr: GatewayAddr::parse(GW).unwrap(),
            token: "t".to_owned().into(),
        },
        resumed: false,
    }
}

impl Alone {
    async fn stop(mut self) {
        self.stop.cancel();
        while self.tasks.join_next().await.is_some() {}
    }
}

/// Review M2d and L6: a fresh link empties the store (nothing from before survives), and a
/// write answered after it is dropped and counted.
#[tokio::test(start_paused = true)]
async fn a_fresh_link_starts_empty_and_drops_late_writes() {
    let routes = HttpRoutes::fogar();
    let hub = EventsHub::new();
    hub.install(&routes);
    hub.seed_conversation("c-1", "Serie");
    routes.set_delayed("POST", "/conversations", 500, "{}", Duration::from_secs(5));
    let a = alone(&routes);
    a.in_tx.send(fresh_up()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(a.chat.borrow().list.len(), 1);
    let (tx, rx) = oneshot::channel();
    a.cmd_tx
        .send(Command::SendText("hola".into(), tx))
        .await
        .unwrap();
    assert_eq!(rx.await.unwrap(), Ok(()));
    // The list fails from now on: whatever is shown after the fresh link is new.
    routes.set("GET", "/conversations", 500, "{}");
    a.in_tx.send(fresh_up()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(a.chat.borrow().list.is_empty(), "the old list went");
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(a.stats.snapshot().rest_stale_writes, 1);
    assert!(
        a.chat.borrow().open.is_none(),
        "the late failure is not shown"
    );
    let mut v = ConversationView::default();
    assert!(
        !a.chat.borrow().render(&mut v),
        "nothing unsent shown either"
    );
    a.stop().await;
}

// ---- PR 4a final review -----------------------------------------------------------------

const READ_ONLY_ME: &str = r#"{"user":{"id":"u","username":"s","display_name":"S","role":"member"},"scopes":["conversations:read"]}"#;

/// Security M1: a flood of `resync` on a read-only account never runs more than one
/// `/auth/me` at a time (`queues`), even when the gateway never answers it.
#[tokio::test(start_paused = true)]
async fn a_resync_flood_runs_one_auth_me_at_a_time() {
    let routes = HttpRoutes::fogar();
    let hub = EventsHub::new();
    hub.install(&routes);
    hub.seed_conversation("c-1", "Serie");
    routes.set("GET", "/auth/me", 200, READ_ONLY_ME);
    let a = alone(&routes);
    a.in_tx.send(fresh_up()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let me = || {
        routes
            .seen()
            .iter()
            .filter(|r| r.method == "GET" && r.path == "/auth/me")
            .count()
    };
    assert_eq!(me(), 1, "the fresh link's");
    routes.set_delayed(
        "GET",
        "/auth/me",
        200,
        READ_ONLY_ME,
        Duration::from_secs(3600),
    );
    for _ in 0..1000 {
        a.in_tx.send(Inbound::Resync).await.unwrap();
    }
    // Well within `http_timeout`: the first resync's call still runs.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(me(), 2, "one in flight, the rest folded");
    a.stop().await;
}

/// Review M-2: a create that fails after the person opened another conversation is never
/// shown there (counted only); a send meanwhile is refused busy (the reply is dropped).
#[tokio::test(start_paused = true)]
async fn a_failed_create_never_shows_in_another_conversation() {
    let mut r = rig_with(|hub, routes| {
        hub.seed_conversation("c-1", "Serie");
        routes.set_delayed("POST", "/conversations", 500, "{}", Duration::from_secs(2));
    });
    r.ready().await;
    assert_eq!(r.send("hola").await, Some(Ok(())));
    assert_eq!(r.send("otra").await, None, "busy while the create runs");
    r.open("c-1").await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(r.health.chat().open_id(), Some("c-1"));
    assert!(failed(&r).is_empty(), "{:?}", r.entries());
    assert!(r.health.snapshot().stats.rest_failures >= 1);
    assert_eq!(r.calls("POST", "/conversations").len(), 1);
    r.stop().await;
}

/// Review M-2: a create that succeeds takes the view, even if another conversation was
/// opened meanwhile (the person's text started it).
#[tokio::test(start_paused = true)]
async fn a_created_conversation_takes_the_view() {
    let mut r = rig_with(|hub, routes| {
        hub.seed_conversation("c-1", "Serie");
        routes.set_delayed(
            "POST",
            "/conversations",
            201,
            r#"{"id":"c-new","agent":"main","created_at":"x","title":null}"#,
            Duration::from_secs(2),
        );
    });
    r.ready().await;
    assert_eq!(r.send("hola").await, Some(Ok(())));
    r.open("c-1").await;
    eventually("switched", || r.health.chat().open_id() == Some("c-new")).await;
    assert_eq!(r.health.conversation().id.as_deref(), Some("c-new"));
    r.stop().await;
}

/// Review L8: stop with a conversation open but no link is answered honestly (nothing was
/// sent), not as if it had been; with nothing open it is simply handled.
#[tokio::test(start_paused = true)]
async fn cancel_while_offline_says_so() {
    let routes = HttpRoutes::fogar();
    let a = alone(&routes);
    let ask = |make: fn(oneshot::Sender<bool>) -> Command| {
        let (tx, rx) = oneshot::channel();
        let cmd_tx = a.cmd_tx.clone();
        async move {
            cmd_tx.send(make(tx)).await.unwrap();
            rx.await.unwrap()
        }
    };
    assert!(ask(Command::Cancel).await, "nothing open: handled");
    let (tx, rx) = oneshot::channel();
    a.cmd_tx
        .send(Command::Open("c-1".into(), tx))
        .await
        .unwrap();
    rx.await.unwrap();
    assert!(!ask(Command::Cancel).await, "open, no link");
    assert!(routes.seen().is_empty(), "nothing was sent");
    a.stop().await;
}

// ---- the engine alone, fed directly (no link) --------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[(self.next() % xs.len() as u64) as usize]
    }
}

/// Random `/events` data, valid and not.
fn random_event_json(r: &mut Rng) -> String {
    let id = *r.pick(&["t-1", "t-2", "", "x"]);
    let text = *r.pick(&["", "pon la serie", "hola", "  "]);
    let conv = *r.pick(&["c-1", "c-2", ""]);
    match r.next() % 10 {
        0 => format!(
            r#"{{"type":"message","id":"m","conversation":"{conv}","text":"{text}","state":"delivered","from":{{"source":"device","self":false}}}}"#
        ),
        1 => format!(
            r#"{{"type":"turn_start","turn_id":"{id}","conversation":"{conv}","origin":"push"}}"#
        ),
        2 => format!(
            r#"{{"type":"token","turn_id":"{id}","conversation":"{conv}","text":"{text}"}}"#
        ),
        3 => format!(r#"{{"type":"turn_end","turn_id":"{id}","reason":"done"}}"#),
        4 => r#"{"type":"audio","turn_id":"t-1","data":"AAAA"}"#.into(),
        5 => r#"{"type":"error","code":"x","message":"y"}"#.into(),
        6 => r#"{"type":"ready","protocol":5}"#.into(),
        7 => r#"{"type":"start_recording","open_mic":true}"#.into(),
        8 => r#"not json at all {{{"#.into(),
        _ => r#"{"type":"resync"}"#.into(),
    }
}

/// «Cualquier secuencia aleatoria de mensajes del gateway … deja FakeMic::opens() == 0», over the
/// `/events` vocabulary: random events, link ups and downs, re-fetches, sign-outs, texts,
/// cancels, capture ends and permission requests never open the microphone nor sound the
/// speaker, and every byte charge is released.
#[tokio::test(start_paused = true)]
async fn no_gateway_sequence_opens_the_microphone_or_sounds() {
    for seed in 1..=48u64 {
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let clock = Arc::new(TestClock::new());
        let cfg = Arc::new(Config::default());
        let stats = Arc::new(Stats::default());
        let (in_tx, in_rx) = mpsc::channel(queues::INBOUND);
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(queues::ENGINE_COMMANDS);
        let (conv_tx, _conv) = watch::channel(ConversationView::default());
        let (chat_tx, _chat) = watch::channel(Chat::default());
        let (list_tx, _list) = watch::channel(Vec::new());
        let engine = Engine {
            clock,
            cfg: cfg.clone(),
            stats: stats.clone(),
            http: Arc::new(crate::NoHttp),
            in_rx,
            cmd_rx,
            conv: conv_tx,
            chat: chat_tx,
            list: list_tx,
        };
        let mic = FakeMic::healthy();
        let speaker = FakeSpeaker::healthy();
        let capture = CaptureGate::new(Arc::new(mic.clone()), &cfg, stats.clone());
        let playback = PlaybackGate::new(Arc::new(speaker.clone()), stats);
        let stop = CancellationToken::new();
        let mut tasks = JoinSet::new();
        tasks.spawn(engine.run(capture, playback, stop.clone()));
        let budget = ByteBudget::new(1 << 20);
        let up = || Inbound::Up {
            api: Api {
                addr: GatewayAddr::parse(GW).unwrap(),
                token: "t".to_owned().into(),
            },
            resumed: false,
        };
        in_tx.send(up()).await.unwrap();
        for _ in 0..60 {
            match r.next() % 10 {
                0 => in_tx.send(up()).await.unwrap(),
                1 => in_tx.send(Inbound::Down).await.unwrap(),
                2 => in_tx.send(Inbound::Resync).await.unwrap(),
                3 => in_tx.send(Inbound::SignedOut).await.unwrap(),
                4 => match r.next() % 5 {
                    0 => {
                        let (tx, _rx) = oneshot::channel();
                        let _ = cmd_tx.try_send(Command::SendText("hola".into(), tx));
                    }
                    1 => {
                        let (tx, _rx) = oneshot::channel();
                        let _ = cmd_tx.try_send(Command::Cancel(tx));
                    }
                    2 => {
                        let (tx, _rx) = oneshot::channel();
                        let _ = cmd_tx.try_send(Command::CaptureEnd(CaptureThen::Send, tx));
                    }
                    3 => {
                        let (tx, _rx) = oneshot::channel();
                        let _ = cmd_tx.try_send(Command::Open("c-1".into(), tx));
                    }
                    _ => {
                        let (tx, rx) = oneshot::channel();
                        cmd_tx
                            .send(Command::RequestMicPermission(tx))
                            .await
                            .unwrap();
                        let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
                    }
                },
                _ => {
                    let json = random_event_json(&mut r);
                    let charge = budget.try_charge(json.len()).unwrap();
                    let ev = Inbound::Event {
                        ev: decode_event(&json),
                        id: Some(r.next() % 100),
                        bytes: json.len(),
                        charge,
                    };
                    in_tx.send(ev).await.unwrap();
                }
            }
            if r.next().is_multiple_of(4) {
                tokio::time::sleep(Duration::from_millis(r.next() % 20_000)).await;
            }
            tokio::task::yield_now().await;
            assert_eq!(mic.opens(), 0, "seed {seed}");
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(mic.opens(), 0, "seed {seed}");
        assert_eq!(speaker.snapshot().opens, 0, "seed {seed}");
        assert_eq!(budget.in_use(), 0, "seed {seed}: every charge released");
        stop.cancel();
        while tasks.join_next().await.is_some() {}
    }
}
