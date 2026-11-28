//! The `/events` hub: a stateful fake chat gateway (client-protocol §5, §14, §16 at the pinned
//! commit), served in memory (paused time) and over a real socket.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use fogar_ports::gateway_http::{GatewayHttp, HttpCall, PathId, ResponseBody};
use fogar_proto::sse::{SseItem, SseReader};
use fogar_testkit::{EventsHub, FakeGatewayHttp, HttpRoutes, MockHttpServer, Scope};
use secrecy::SecretString;
use secrecy::zeroize::Zeroizing;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::Instant;

fn token() -> SecretString {
    SecretString::from("tok".to_owned())
}

fn rig() -> (EventsHub, FakeGatewayHttp) {
    let hub = EventsHub::new();
    let routes = HttpRoutes::fogar();
    hub.install(&routes);
    (hub, FakeGatewayHttp::new(routes))
}

fn id(s: &str) -> PathId {
    PathId::new(s).unwrap()
}

/// One decoded SSE item: `(id, json)` or `None` for a comment.
type Item = Option<(Option<String>, Value)>;
type Ev = (Option<String>, Value);

struct Client<'a> {
    http: &'a FakeGatewayHttp,
    body: <FakeGatewayHttp as GatewayHttp>::Body,
    reader: SseReader,
    started: Instant,
}

async fn open_with<'a>(http: &'a FakeGatewayHttp, audio: bool, last: Option<&str>) -> Client<'a> {
    let r = http
        .open_stream(
            &http.addr(),
            &HttpCall::Events { audio },
            &token(),
            last,
            None,
        )
        .await
        .unwrap();
    assert_eq!(r.status, 200);
    Client {
        http,
        body: r.body,
        reader: SseReader::new(),
        started: Instant::now(),
    }
}

async fn open<'a>(http: &'a FakeGatewayHttp, last: Option<&str>) -> Client<'a> {
    open_with(http, false, last).await
}

impl Client<'_> {
    /// The next batch of items; `None` at EOF.
    async fn next(&mut self) -> Option<Vec<Item>> {
        let chunk = self.body.chunk().await.unwrap()?;
        let mut out = Vec::new();
        self.reader.feed(&chunk, &mut |it| match it {
            SseItem::Event { id, data } => out.push(Some((
                id.map(str::to_owned),
                serde_json::from_str(data).unwrap(),
            ))),
            SseItem::Comment => out.push(None),
            other => panic!("unexpected {other:?}"),
        });
        Some(out)
    }
    /// Reads until `n` events (comments skipped); returns them.
    async fn events(&mut self, n: usize) -> Vec<Ev> {
        let mut got = Vec::new();
        while got.len() < n {
            let batch = self.next().await.expect("stream ended early");
            got.extend(batch.into_iter().flatten());
        }
        got
    }
    async fn until_type(&mut self, ty: &str) -> Vec<Ev> {
        let mut got = Vec::new();
        loop {
            let batch = self.next().await.expect("stream ended early");
            for e in batch.into_iter().flatten() {
                let done = e.1["type"] == ty;
                got.push(e);
                if done {
                    return got;
                }
            }
        }
    }
    async fn post(&self, call: HttpCall, body: Value) -> (u16, Value) {
        call_json(self.http, call, Some(body)).await
    }
}

async fn call_json(http: &FakeGatewayHttp, call: HttpCall, body: Option<Value>) -> (u16, Value) {
    let b = body.map(|v| Zeroizing::new(serde_json::to_vec(&v).unwrap()));
    let r = http
        .request(&http.addr(), &call, Some(&token()), b)
        .await
        .unwrap();
    let v = if r.body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&r.body).unwrap()
    };
    (r.status, v)
}

fn post_msg(conv: &str) -> HttpCall {
    HttpCall::PostMessage { id: id(conv) }
}

fn types(evs: &[Ev]) -> Vec<&str> {
    evs.iter().map(|e| e.1["type"].as_str().unwrap()).collect()
}

fn num(e: &Ev) -> u64 {
    e.0.as_ref().unwrap().parse().unwrap()
}

#[tokio::test(start_paused = true)]
async fn post_message_produces_message_and_turn() {
    let (hub, http) = rig();
    hub.seed_conversation("c1", "Hola");
    hub.set_reply(&["uno ", "dos"], Duration::from_millis(10));
    let mut c = open(&http, None).await;
    let ready = c.events(1).await;
    assert_eq!(ready[0].1["type"], "ready");
    assert_eq!(ready[0].1["protocol"], 5);
    assert!(ready[0].0.is_some(), "a fresh stream's ready carries an id");

    let (st, body) = c.post(post_msg("c1"), json!({"text": "hola"})).await;
    assert_eq!(st, 202);
    let mid = body["message_id"].as_str().unwrap().to_owned();

    let evs = c.until_type("turn_end").await;
    assert_eq!(
        types(&evs),
        ["message", "turn_start", "token", "token", "turn_end"]
    );
    assert_eq!(evs[0].1["id"], json!(mid));
    assert_eq!(evs[0].1["state"], "delivered");
    assert_eq!(evs[0].1["from"]["self"], true);
    assert_eq!(evs[0].1["conversation"], "c1");
    assert_eq!(evs[1].1["origin"], "user");
    assert_eq!(evs[1].1["by"]["self"], true);
    assert_eq!(evs[4].1["reason"], "done");
    let ids: Vec<u64> = evs.iter().map(num).collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "ids only go up");
    assert!(
        ids.windows(2).any(|w| w[1] - w[0] > 1),
        "ids are not contiguous: a client cannot assume +1"
    );
    // The reply is in history under ids of its own.
    let (_, h) = call_json(
        &http,
        HttpCall::GetMessages {
            id: id("c1"),
            before: None,
            limit: 50,
        },
        None,
    )
    .await;
    assert_eq!(h["messages"].as_array().unwrap().len(), 2);
    assert_eq!(h["messages"][1]["role"], "assistant");
    assert_eq!(h["messages"][1]["text"], "uno dos");
    assert_ne!(h["messages"][0]["id"], json!(mid));
}

#[tokio::test(start_paused = true)]
async fn pending_then_delivered_with_a_turn_running() {
    let (hub, http) = rig();
    hub.seed_conversation("c1", "Hola");
    hub.set_reply(&["a"], Duration::from_secs(1));
    let mut c = open(&http, None).await;
    c.events(1).await;
    c.post(post_msg("c1"), json!({"text": "uno"})).await;
    c.events(2).await; // message delivered + turn_start
    let (_, second) = c.post(post_msg("c1"), json!({"text": "dos"})).await;
    let second = second["message_id"].as_str().unwrap().to_owned();
    let pend = c.events(1).await;
    assert_eq!(pend[0].1["state"], "pending");
    assert_eq!(pend[0].1["id"], json!(second));
    // GET shows it pending, with the turn in flight and `as_of`.
    let (_, live) = call_json(&http, HttpCall::GetConversation { id: id("c1") }, None).await;
    assert_eq!(live["pending"][0]["id"], json!(second));
    assert_eq!(live["turn"]["type"], "turn_start");
    assert!(live["as_of"].as_u64().unwrap() > 0);
    // Then: token, turn_end, message delivered, turn_start.
    let rest = c.until_type("turn_start").await;
    assert_eq!(types(&rest), ["token", "turn_end", "message", "turn_start"]);
    assert_eq!(rest[2].1["state"], "delivered");
    assert_eq!(rest[2].1["id"], json!(second));
    // Withdrawing what is no longer pending is 404 not_pending.
    let (st, b) = call_json(
        &http,
        HttpCall::Withdraw {
            id: id("c1"),
            message: id(&second),
        },
        None,
    )
    .await;
    assert_eq!((st, &b["detail"]), (404, &json!("not_pending")));
}

#[tokio::test(start_paused = true)]
async fn withdraw_and_cancel() {
    let (hub, http) = rig();
    hub.seed_conversation("c1", "x");
    hub.set_reply(&["a", "b", "c"], Duration::from_secs(1));
    let mut c = open(&http, None).await;
    c.events(1).await;
    c.post(post_msg("c1"), json!({"text": "uno"})).await;
    let (_, p) = c.post(post_msg("c1"), json!({"text": "dos"})).await;
    let p = p["message_id"].as_str().unwrap().to_owned();
    c.until_type("turn_start").await;
    c.events(1).await; // pending
    let (st, _) = call_json(
        &http,
        HttpCall::Withdraw {
            id: id("c1"),
            message: id(&p),
        },
        None,
    )
    .await;
    assert_eq!(st, 204);
    let w = c.events(1).await;
    assert_eq!(w[0].1["state"], "withdrawn");
    let (_, r) = call_json(&http, HttpCall::Cancel { id: id("c1") }, None).await;
    assert_eq!(r["cancelled"], true);
    let end = c.until_type("turn_end").await;
    assert_eq!(end.last().unwrap().1["reason"], "cancelled");
    let (_, r) = call_json(&http, HttpCall::Cancel { id: id("c1") }, None).await;
    assert_eq!(r["cancelled"], false);
}

#[tokio::test(start_paused = true)]
async fn reconnect_with_last_event_id_gets_exactly_the_missed_events() {
    let (hub, http) = rig();
    hub.seed_conversation("c1", "x");
    hub.set_reply(&["a", "b"], Duration::from_millis(10));
    let mut c = open(&http, None).await;
    c.events(1).await;
    hub.other_device_send("c1", "uno");
    let seen = c.until_type("turn_end").await;
    let last = seen.last().unwrap().0.clone().unwrap();
    hub.drop_streams();
    assert!(c.next().await.is_none(), "dropped streams end");
    // Events while no stream is open are kept for the device.
    hub.other_device_send("c1", "dos");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut c2 = open(&http, Some(&last)).await;
    let got = c2.events(1 + 5).await; // ready + message, turn_start, 2 tokens, turn_end
    assert_eq!(got[0].1["type"], "ready");
    assert!(got[0].0.is_none(), "a resumed ready carries no id");
    assert!(
        got[1..]
            .iter()
            .all(|e| num(e) > last.parse::<u64>().unwrap())
    );
    assert_eq!(got[1].1["text"], "dos");
    assert_eq!(got[1].1["from"]["self"], false);
    assert_eq!(got[5].1["type"], "turn_end");
    // Nothing is replayed twice: resuming from the newest id gets only `ready`.
    let newest = got[5].0.clone().unwrap();
    drop(c2);
    let mut c3 = open(&http, Some(&newest)).await;
    assert_eq!(types(&c3.events(1).await), ["ready"]);
    let more = tokio::time::timeout(Duration::from_secs(1), c3.next()).await;
    assert!(more.is_err(), "nothing but `ready` was missed");
    assert_eq!(hub.subscribers(), 1);
}

#[tokio::test(start_paused = true)]
async fn unknown_last_event_id_gets_resync() {
    let (hub, http) = rig();
    hub.seed_conversation("c1", "x");
    for last in ["1", "garbage", "99999999999999999"] {
        let mut c = open(&http, Some(last)).await;
        let got = c.events(2).await;
        assert_eq!(types(&got), ["ready", "resync"], "last-event-id {last}");
        assert!(got[0].0.is_some(), "both carry the current id");
        assert_eq!(got[0].0, got[1].0);
    }
    // forget_buffer: a once-valid id falls out.
    let mut c = open(&http, None).await;
    let last = c.events(1).await[0].0.clone().unwrap();
    hub.other_device_send("c1", "x");
    hub.forget_buffer();
    let mut c2 = open(&http, Some(&last)).await;
    assert_eq!(c2.events(2).await[1].1["type"], "resync");
}

#[tokio::test(start_paused = true)]
async fn a_long_gap_loses_the_buffer_and_revoking_cuts_streams() {
    let (hub, http) = rig();
    hub.seed_conversation("c1", "x");
    let mut c = open(&http, None).await;
    let last = c.events(1).await[0].0.clone().unwrap();
    hub.other_device_send("c1", "uno");
    drop(c);
    // Within RESUME_S (120 s) the device still resumes.
    tokio::time::sleep(Duration::from_secs(100)).await;
    let mut c = open(&http, Some(&last)).await;
    assert_ne!(types(&c.events(2).await)[1], "resync");
    drop(c);
    // Past it, the device is closed: `resync`.
    tokio::time::sleep(Duration::from_secs(121)).await;
    let mut c = open(&http, Some(&last)).await;
    assert_eq!(types(&c.events(2).await), ["ready", "resync"]);
    // Revoking cuts the open stream (`close_device`).
    hub.revoke();
    assert!(c.next().await.is_none());
    assert_eq!(hub.subscribers(), 0);
}

#[tokio::test(start_paused = true)]
async fn stop_before_the_turn_starts_withdraws_the_message() {
    let (hub, http) = rig();
    hub.seed_conversation("c1", "x");
    let mut c = open(&http, None).await;
    c.events(1).await;
    let (_, m) = c.post(post_msg("c1"), json!({"text": "uno"})).await;
    let mid = m["message_id"].clone();
    let sent = c.events(1).await;
    assert_eq!(sent[0].1["state"], "delivered");
    // Delivered but not started: stop withdraws it, with no turn events.
    let (_, r) = call_json(&http, HttpCall::Cancel { id: id("c1") }, None).await;
    assert_eq!(r["cancelled"], true);
    let w = c.events(1).await;
    assert_eq!(
        (&w[0].1["type"], &w[0].1["state"]),
        (&json!("message"), &json!("withdrawn"))
    );
    assert_eq!(w[0].1["id"], mid);
    let quiet = tokio::time::timeout(Duration::from_secs(10), c.next()).await;
    assert!(quiet.is_err(), "no turn_start / turn_end follows");
    let (_, live) = call_json(&http, HttpCall::GetConversation { id: id("c1") }, None).await;
    assert!(live["turn"].is_null());
    assert_eq!(live["turn_count"], 0);
    let (_, r) = call_json(&http, HttpCall::Cancel { id: id("c1") }, None).await;
    assert_eq!(r["cancelled"], false);
}

#[tokio::test(start_paused = true)]
async fn list_order_changes_when_the_turn_starts() {
    let (hub, http) = rig();
    hub.seed_conversation("c1", "uno");
    hub.seed_conversation("c2", "dos");
    hub.set_reply(&["a"], Duration::from_secs(5));
    let mut c = open(&http, None).await;
    c.events(1).await;
    c.post(post_msg("c1"), json!({"text": "hola"})).await;
    let (_, l) = call_json(&http, HttpCall::ListConversations, None).await;
    assert_eq!(l[0]["id"], "c2", "nothing has talked yet");
    c.until_type("turn_start").await;
    // The turn is still running (5 s before its first token).
    let (_, l) = call_json(&http, HttpCall::ListConversations, None).await;
    assert_eq!(l[0]["id"], "c1");
    assert!(l[0]["last_turn_at"].is_string() && l[1]["last_turn_at"].is_null());
    assert_eq!(l[0]["turn_count"], 1);
    // `last_turn_at` is orderable: a second conversation talking later sorts after it.
    hub.push("c2", "x", false);
    let (_, l2) = call_json(&http, HttpCall::ListConversations, None).await;
    assert!(l2[0]["last_turn_at"].as_str() > l2[1]["last_turn_at"].as_str());
}

#[tokio::test(start_paused = true)]
async fn rotation_ends_at_the_deadline_under_a_backlog() {
    let (hub, http) = rig();
    hub.seed_conversation("c1", "x");
    let mut c = open(&http, None).await;
    for _ in 0..5 {
        hub.push("c1", "x", false);
    }
    tokio::time::sleep(Duration::from_secs(26)).await;
    let mut seen = Vec::new();
    while let Some(b) = c.next().await {
        seen.extend(b.into_iter().flatten().map(|e| e.1["type"].clone()));
    }
    assert_eq!(
        seen,
        [json!("ready")],
        "the backlog waits for the next stream"
    );
    let mut c2 = open(&http, None).await;
    assert_eq!(c2.events(1).await[0].1["type"], "ready");
}

#[tokio::test(start_paused = true)]
async fn pings_and_rotation() {
    let (hub, http) = rig();
    let mut c = open(&http, None).await;
    c.events(1).await;
    let mut pings = 0;
    while let Some(batch) = c.next().await {
        pings += batch.iter().filter(|i| i.is_none()).count();
    }
    // The gateway pings at 15 s and, its wait timing out at 25 s, once more before it ends.
    assert_eq!(pings, 2, "`: ping` at 15 s and at 25 s");
    let took = c.started.elapsed();
    assert!(
        took >= Duration::from_secs(25) && took < Duration::from_secs(26),
        "{took:?}"
    );
    drop(c);
    assert_eq!(hub.subscribers(), 0, "dropping the stream unsubscribes");
}

#[tokio::test(start_paused = true)]
async fn silent_stream_sends_nothing_and_never_ends() {
    let (hub, http) = rig();
    let mut c = open(&http, None).await;
    c.events(1).await;
    hub.go_silent();
    let r = tokio::time::timeout(Duration::from_secs(300), c.next()).await;
    assert!(r.is_err(), "no byte and no EOF while silent");
    // A new stream is healthy again.
    let mut c2 = open(&http, None).await;
    assert_eq!(c2.events(1).await[0].1["type"], "ready");
}

#[tokio::test(start_paused = true)]
async fn push_audio_reaches_only_audio_streams() {
    let (hub, http) = rig();
    hub.seed_conversation("c1", "x");
    let mut text = open(&http, None).await;
    let mut audio = open_with(&http, true, None).await;
    text.events(1).await;
    audio.events(1).await;
    hub.push("c1", "ding", true);
    let t = text.until_type("turn_end").await;
    let a = audio.until_type("turn_end").await;
    assert!(t.iter().all(|e| e.1["type"] != "audio"));
    let au: Vec<_> = a.iter().filter(|e| e.1["type"] == "audio").collect();
    assert_eq!(au.len(), 1);
    assert!(au[0].0.is_none(), "audio has no id");
    assert_eq!(t[0].1["origin"], "push");
    assert!(t[0].1["by"].is_null());
}

#[tokio::test(start_paused = true)]
async fn lists_and_creates_conversations() {
    let (hub, http) = rig();
    hub.seed_conversation("c1", "uno");
    hub.seed_conversation("c2", "dos");
    hub.seed_history("c1", "user", "hola");
    let (st, l) = call_json(&http, HttpCall::ListConversations, None).await;
    assert_eq!(st, 200);
    assert_eq!(l.as_array().unwrap().len(), 2);
    let mut c = open(&http, None).await;
    c.events(1).await;
    let (st, v) = c
        .post(HttpCall::CreateConversation, json!({"message": "hola"}))
        .await;
    assert_eq!(st, 201);
    assert!(v["message_id"].is_string());
    assert!(v["title"].is_null(), "a new conversation has no title yet");
    let (st, bare) = c.post(HttpCall::CreateConversation, json!({})).await;
    assert_eq!(st, 201);
    assert!(bare.as_object().unwrap().contains_key("message_id"));
    assert!(bare["message_id"].is_null());
    hub.seed_history_without_id("c1", "assistant", "sin id");
    let (_, h) = call_json(
        &http,
        HttpCall::GetMessages {
            id: id("c1"),
            before: None,
            limit: 50,
        },
        None,
    )
    .await;
    assert!(h["messages"][1]["id"].is_null());
    let new = v["id"].as_str().unwrap().to_owned();
    c.until_type("turn_end").await;
    let (_, l) = call_json(&http, HttpCall::ListConversations, None).await;
    assert_eq!(
        l[0]["id"],
        json!(new),
        "the one that talked last goes first"
    );
    let (_, a) = call_json(&http, HttpCall::ListAgents, None).await;
    assert!(a["agents"].is_array());
    let (st, _) = call_json(&http, HttpCall::GetConversation { id: id("nope") }, None).await;
    assert_eq!(st, 404);
    let (st, b) = call_json(&http, post_msg("c1"), Some(json!({"text": "  "}))).await;
    assert_eq!((st, &b["detail"]), (422, &json!("empty_message")));
}

#[tokio::test(start_paused = true)]
async fn revoked_is_401_and_scopes_are_403() {
    let (hub, http) = rig();
    hub.seed_conversation("c1", "x");
    let events = |http: &FakeGatewayHttp| {
        let http = http.clone();
        async move {
            http.open_stream(
                &http.addr(),
                &HttpCall::Events { audio: false },
                &token(),
                None,
                None,
            )
            .await
            .unwrap()
            .status
        }
    };
    hub.revoke();
    assert_eq!(events(&http).await, 401);
    let (st, b) = call_json(&http, HttpCall::ListConversations, None).await;
    assert_eq!((st, &b["detail"]), (401, &json!("not_authenticated")));
    hub.restore();
    hub.missing_scope(Scope::Read);
    assert_eq!(events(&http).await, 403);
    hub.restore();
    hub.missing_scope(Scope::Write);
    let (st, b) = call_json(&http, post_msg("c1"), Some(json!({"text": "x"}))).await;
    assert_eq!((st, &b["detail"]), (403, &json!("missing_scope")));
    let (st, _) = call_json(&http, HttpCall::ListConversations, None).await;
    assert_eq!(st, 200, "reads still work without the write scope");
}

#[tokio::test]
async fn the_same_hub_over_a_real_socket() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let hub = EventsHub::new();
    let routes = HttpRoutes::fogar();
    hub.install(&routes);
    hub.seed_conversation("c1", "x");
    let server = MockHttpServer::start(routes).await.unwrap();
    // Raw HTTP: the testkit does not depend on fogar-host.
    let mut s = tokio::net::TcpStream::connect(server.addr().authority())
        .await
        .unwrap();
    s.write_all(b"GET /events HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\n\r\n")
        .await
        .unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    while !String::from_utf8_lossy(&got).contains("\"ready\"") {
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0, "stream ended before ready");
        got.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&got);
    assert!(text.starts_with("HTTP/1.1 200"));
    assert!(text.contains("retry: 1000"));
    assert_eq!(hub.subscribers(), 1);
    drop(s);
}
