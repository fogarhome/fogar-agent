//! [`EventsHub`]: a stateful fake chat gateway for one device (5.0 client-protocol §5, §14 and
//! §16 at `SPEC_GATEWAY_COMMIT`). It answers `GET /events` (SSE), the conversation reads and the
//! chat writes, behind the same [`HttpRoutes`] as every other fake, so `FakeGatewayHttp`
//! (paused time) and `MockHttpServer` (real socket) serve the same behaviour.
//!
//! Followed literally from `events_routes.py` / `services/events.py` / `conversation_routes.py`:
//! `retry: 1000`; `ready` first (with the current id on a fresh stream, WITHOUT an id when the
//! stream resumes); a numbered ring of the last [`RING_MAX`] events; replay after
//! `Last-Event-ID`, or `resync` (with the current id, after `ready`) when the id is out of the
//! ring or not a number; `: ping` after `ping_every` without events; the stream ends at
//! `stream_for`; a subscriber whose queue overflows is cut (it resumes from the ring); `audio`
//! events carry no id and reach only `?audio=1` streams; ids only go up.
//!
//! Also literal: a quiet stream ends with one more `: ping` (the gateway's wait times out at the
//! deadline); a device with no stream for 120 s (`RESUME_S`) loses its buffer; `revoke` cuts the
//! open streams (`close_device`); the `message` is published before the 202 returns, and a
//! pending message is announced `delivered` when its run opens (`channel.py`).
//! NOT modelled: several devices (one device id, `other_device_send` is a second sender),
//! private conversations, `allowed_agents`, `push_enabled`, tool calls and `turn_progress`,
//! `/speech`, `speak`, steering (`absorbed`), the 502/503 send errors, the title cache.
//!
//! Keep an `EventsHub` handle alive while the routes are in use: the routes hold the hub weakly,
//! so once the last handle drops they answer 404 and the turn drivers stop.
//!
//! Everything is bounded: [`QUEUE_MAX`] events per stream, [`RING_MAX`] in the ring, 1000 history
//! messages per conversation. No task of its own except the per-conversation turn driver, which
//! lives in an owned `JoinSet` (aborted when the last hub handle drops).
//!
//! Anything the documents do not say is marked GUESSED below.

use super::http::{HookReply, HttpRoutes, RouteHook, SeenRequest};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::mpsc,
    task::JoinSet,
    time::{Instant, sleep, sleep_until},
};
use tokio_util::sync::CancellationToken;

/// Events a device keeps for resuming (`BUFFER_MAX`).
pub const RING_MAX: usize = 2000;
/// Pending events of one stream before its reader is cut (`QUEUE_MAX`).
pub const QUEUE_MAX: usize = 1000;
/// How long a device keeps its buffer after its last stream went (`RESUME_S`).
const RESUME: Duration = Duration::from_secs(120);
/// History kept per conversation (`chat.history` window).
const HISTORY_MAX: usize = 1000;

/// Which scope a test takes away from the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `conversations:read`: `/events` and the reads.
    Read,
    /// `conversations:write`: sending, withdrawing, cancelling, creating.
    Write,
}

/// Hub knobs.
#[derive(Debug, Clone)]
pub struct HubConfig {
    /// No event for this long: `: ping` (gateway `KEEPALIVE_S`, 15 s).
    pub ping_every: Duration,
    /// A stream ends this long after it opened (`STREAM_MAX_S`, 25 s).
    pub stream_for: Duration,
    /// The scripted reply: one `token` per entry, `token_gap` before each.
    pub reply: Vec<String>,
    pub token_gap: Duration,
    /// From a message queued (or the turn before ending) to its `turn_start`: the window in
    /// which "stop" withdraws the message instead of stopping a turn. GUESSED: the gateway's
    /// real delay is OpenClaw's; non-zero so the window exists.
    pub first_token_after: Duration,
    /// This device (`self` in `from`/`by` is relative to it).
    pub device_id: String,
    pub device_name: String,
    /// GUESSED misbehaviour switch: deliver `audio` events to streams that did not ask
    /// (`?audio=1`), to test that a client drops them.
    pub audio_to_all: bool,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            ping_every: Duration::from_secs(15),
            stream_for: Duration::from_secs(25),
            reply: vec!["Hola".into(), " mundo".into()],
            token_gap: Duration::ZERO,
            first_token_after: Duration::from_millis(200),
            device_id: "dev-mac".into(),
            device_name: "Mac".into(),
            audio_to_all: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MsgState {
    Pending,
    Delivered,
    Withdrawn,
}

impl MsgState {
    fn as_str(self) -> &'static str {
        match self {
            MsgState::Pending => "pending",
            MsgState::Delivered => "delivered",
            MsgState::Withdrawn => "withdrawn",
        }
    }
}

#[derive(Debug, Clone)]
struct Msg {
    /// The run id (the `message` event's `id`, the `message_id` of the 202).
    id: String,
    text: String,
    device_id: String,
    device_name: String,
    state: MsgState,
}

#[derive(Debug)]
struct Turn {
    id: String,
    origin: &'static str,
    by: Option<(String, String)>,
    text: String,
    cancel: CancellationToken,
    /// Set before `cancel` fires: `cancelled`.
    reason: Option<&'static str>,
}

#[derive(Debug)]
struct HistMsg {
    /// `None`: OpenClaw gave the message no id (the field is optional).
    id: Option<String>,
    role: &'static str,
    text: String,
}

#[derive(Debug)]
struct Conv {
    id: String,
    agent: String,
    /// `None`: OpenClaw has no title yet (a new conversation).
    title: Option<String>,
    /// Ordering of the list: the last to talk goes first.
    talked_seq: u64,
    turn_count: u32,
    history: VecDeque<HistMsg>,
    queue: VecDeque<Msg>,
    turn: Option<Turn>,
    /// A driver task owns the queue (a turn runs or is about to).
    driver: bool,
}

#[derive(Debug)]
struct Sub {
    id: u64,
    tx: mpsc::Sender<String>,
    audio: bool,
    silent: Arc<AtomicBool>,
}

#[derive(Debug, Default)]
struct Auth {
    revoked: bool,
    no_read: bool,
    no_write: bool,
}

#[derive(Debug)]
struct State {
    cfg: HubConfig,
    last_id: u64,
    ring: VecDeque<(u64, String)>,
    /// The newest id the ring no longer holds: a resume from below it is a `resync`.
    floor: u64,
    subs: Vec<Sub>,
    next_sub: u64,
    /// Since when no stream is open (`RESUME_S`).
    idle_since: Option<Instant>,
    convs: Vec<Conv>,
    counter: u64,
    auth: Auth,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    drivers: Mutex<JoinSet<()>>,
    opens: std::sync::atomic::AtomicUsize,
}

/// A handle to the hub; clones share it.
#[derive(Debug, Clone)]
pub struct EventsHub {
    shared: Arc<Shared>,
}

impl Default for EventsHub {
    fn default() -> Self {
        Self::new()
    }
}

fn sse(data: &Value, id: Option<u64>) -> String {
    let head = id.map_or_else(String::new, |i| format!("id: {i}\n"));
    format!("{head}data: {data}\n\n")
}

fn detail(status: u16, d: &str) -> HookReply {
    HookReply::Plain {
        status,
        body: json!({ "detail": d }).to_string().into_bytes(),
    }
}

fn ok(status: u16, v: &Value) -> HookReply {
    HookReply::Plain {
        status,
        body: v.to_string().into_bytes(),
    }
}

impl State {
    fn next_n(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{prefix}-{}", self.counter)
    }

    fn conv_ix(&self, id: &str) -> Option<usize> {
        self.convs.iter().position(|c| c.id == id)
    }

    /// Gives `data` the next id, keeps it in the ring and queues it to every stream. A stream
    /// whose queue is full is cut (it will resume from the ring).
    fn publish(&mut self, data: &Value) {
        // Ids only go up, not by one (the gateway's are µs clocks): 1, 2 or 3 at a time, so a
        // client cannot assume contiguity.
        self.last_id += 1 + self.last_id % 3;
        let id = self.last_id;
        let line = sse(data, Some(id));
        if self.ring.len() == RING_MAX
            && let Some((old, _)) = self.ring.pop_front()
        {
            self.floor = old;
        }
        self.ring.push_back((id, line.clone()));
        self.subs.retain(|s| s.tx.try_send(line.clone()).is_ok());
        self.note_idle();
    }

    /// Starts the `RESUME_S` clock when the last stream is gone.
    fn note_idle(&mut self) {
        if self.subs.is_empty() && self.idle_since.is_none() {
            self.idle_since = Some(Instant::now());
        }
    }

    /// Forgets the ring (`close_device`, `RESUME_S`): resuming from an earlier id is a `resync`.
    fn forget(&mut self) {
        self.ring.clear();
        self.floor = self.last_id;
    }

    /// No id, no ring (audio): only to the streams that asked for it.
    fn publish_live(&mut self, data: &Value) {
        let line = sse(data, None);
        let all = self.cfg.audio_to_all;
        self.subs
            .retain(|s| !(s.audio || all) || s.tx.try_send(line.clone()).is_ok());
    }

    fn msg_json(&self, m: &Msg, conv: &str) -> Value {
        json!({
            "type": "message",
            "id": m.id,
            "conversation": conv,
            "text": m.text,
            "from": {
                "source": "device",
                "device_id": m.device_id,
                "name": m.device_name,
                "self": m.device_id == self.cfg.device_id,
            },
            "state": m.state.as_str(),
        })
    }

    fn turn_json(&self, t: &Turn, conv: &str) -> Value {
        let by = t
            .by
            .as_ref()
            .map(|(d, n)| json!({ "device_id": d, "name": n, "self": *d == self.cfg.device_id }));
        json!({
            "type": "turn_start",
            "turn_id": t.id,
            "conversation": conv,
            "origin": t.origin,
            "by": by,
        })
    }

    fn conv_json(&self, c: &Conv) -> Value {
        json!({
            "id": c.id,
            "client_id": self.cfg.device_id,
            "client_name": self.cfg.device_name,
            "agent": c.agent,
            "kind": "created",
            "day": null,
            "created_at": "2026-11-27T09:00:00+00:00",
            // Set when a turn STARTS (`note_turn`); distinct and orderable (nanoseconds).
            "last_turn_at": (c.turn_count > 0)
                .then(|| format!("2026-11-27T10:00:00.{:09}+00:00", c.talked_seq)),
            "turn_count": c.turn_count,
            "title": c.title,
            "owner_user_id": null,
            "message_id": null,
        })
    }

    fn new_conv(&mut self, id: String, agent: &str, title: Option<&str>) -> usize {
        self.counter += 1;
        let seq = self.counter;
        self.convs.push(Conv {
            id,
            agent: agent.into(),
            title: title.map(Into::into),
            talked_seq: seq,
            turn_count: 0,
            history: VecDeque::new(),
            queue: VecDeque::new(),
            turn: None,
            driver: false,
        });
        self.convs.len() - 1
    }

    fn add_history(&mut self, ix: usize, role: &'static str, text: &str) {
        let id = Some(self.next_n("hist"));
        self.push_history(ix, id, role, text);
    }

    fn push_history(&mut self, ix: usize, id: Option<String>, role: &'static str, text: &str) {
        let h = &mut self.convs[ix].history;
        if h.len() == HISTORY_MAX {
            h.pop_front();
        }
        h.push_back(HistMsg {
            id,
            role,
            text: text.into(),
        });
    }

    /// Queues a message of `device`; `true` when a driver must be spawned for it.
    /// (`interrupt` / "send now" is PR 4b: not modelled, the field is ignored.)
    fn send(&mut self, ix: usize, device: (String, String), text: &str) -> (String, bool) {
        let id = self.next_n("run");
        let busy = self.convs[ix].driver;
        let state = if busy {
            MsgState::Pending
        } else {
            MsgState::Delivered
        };
        let m = Msg {
            id: id.clone(),
            text: text.into(),
            device_id: device.0,
            device_name: device.1,
            state,
        };
        let conv_id = self.convs[ix].id.clone();
        let ev = self.msg_json(&m, &conv_id);
        let c = &mut self.convs[ix];
        c.queue.push_back(m);
        let spawn = !std::mem::replace(&mut c.driver, true);
        self.publish(&ev);
        (id, spawn)
    }

    /// The driver's step: takes the next message and opens its turn. `None`: queue empty.
    fn start_turn(&mut self, conv_id: &str) -> Option<(Vec<String>, Duration, CancellationToken)> {
        let ix = self.conv_ix(conv_id)?;
        let Some(mut m) = self.convs[ix].queue.pop_front() else {
            self.convs[ix].driver = false;
            return None;
        };
        if m.state == MsgState::Pending {
            // channel.py: a pending message becomes `delivered` when its run opens (announced).
            m.state = MsgState::Delivered;
            let ev = self.msg_json(&m, conv_id);
            self.publish(&ev);
        }
        let turn = Turn {
            id: self.next_n("turn"),
            origin: "user",
            by: Some((m.device_id.clone(), m.device_name.clone())),
            text: String::new(),
            cancel: CancellationToken::new(),
            reason: None,
        };
        let ev = self.turn_json(&turn, conv_id);
        let cancel = turn.cancel.clone();
        // `note_turn` (conversations.py): the conversation talked when the turn STARTS.
        self.counter += 1;
        let n = self.counter;
        let c = &mut self.convs[ix];
        c.turn = Some(turn);
        c.turn_count += 1;
        c.talked_seq = n;
        self.add_history(ix, "user", &m.text);
        self.publish(&ev);
        Some((self.cfg.reply.clone(), self.cfg.token_gap, cancel))
    }

    fn token(&mut self, conv_id: &str, tok: &str) {
        let Some(ix) = self.conv_ix(conv_id) else {
            return;
        };
        let Some(t) = self.convs[ix].turn.as_mut() else {
            return;
        };
        t.text.push_str(tok);
        let ev = json!({"type": "token", "turn_id": t.id, "conversation": conv_id, "text": tok});
        self.publish(&ev);
    }

    /// Ends the turn; `true` when more messages are queued (the driver goes on).
    fn end_turn(&mut self, conv_id: &str) -> bool {
        let Some(ix) = self.conv_ix(conv_id) else {
            return false;
        };
        let Some(t) = self.convs[ix].turn.take() else {
            return false;
        };
        let reason = t.reason.unwrap_or("done");
        if reason == "done" {
            self.add_history(ix, "assistant", &t.text);
        }
        let ev = json!({
            "type": "turn_end", "turn_id": t.id, "conversation": conv_id, "reason": reason,
        });
        let more = !self.convs[ix].queue.is_empty();
        self.convs[ix].driver = more;
        self.publish(&ev);
        more
    }
}

async fn drive(shared: Weak<Shared>, conv_id: String) {
    loop {
        let Some(s) = shared.upgrade() else { return };
        let lead = s.state.lock().unwrap().cfg.first_token_after;
        drop(s);
        // The window in which "stop" withdraws the message (no turn yet).
        sleep(lead).await;
        let step = {
            let Some(s) = shared.upgrade() else { return };
            s.state.lock().unwrap().start_turn(&conv_id)
        };
        let Some((reply, gap, cancel)) = step else {
            return;
        };
        for tok in reply {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = sleep(gap) => {}
            }
            let Some(s) = shared.upgrade() else { return };
            s.state.lock().unwrap().token(&conv_id, &tok);
        }
        let Some(s) = shared.upgrade() else { return };
        // An idle conversation has no driver: a message sent now is `delivered`, not `pending`.
        if !s.state.lock().unwrap().end_turn(&conv_id) {
            return;
        }
    }
}

impl Shared {
    fn spawn_driver(self: &Arc<Self>, conv_id: &str) {
        let mut set = self.drivers.lock().unwrap();
        while set.try_join_next().is_some() {} // reap: the set stays small
        set.spawn(drive(Arc::downgrade(self), conv_id.to_owned()));
    }
}

/// One open `/events` response: bytes for the transport, `None` at the end of the stream.
/// Dropping it unsubscribes.
#[derive(Debug)]
pub(crate) struct LiveStream {
    shared: Weak<Shared>,
    sub_id: u64,
    head: VecDeque<String>,
    rx: mpsc::Receiver<String>,
    deadline: Instant,
    ping_every: Duration,
    stream_for: Duration,
    next_ping: Instant,
    silent: Arc<AtomicBool>,
    /// The deadline passed: the next call is the end.
    ended: bool,
}

impl LiveStream {
    /// The next bytes. Cancel-safe: the timers and the queue live in `self`, and `recv` loses
    /// nothing when dropped.
    pub(crate) async fn next(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(h) = self.head.pop_front() {
                return Some(h.into_bytes());
            }
            if self.ended {
                return None;
            }
            // The deadline comes first: constant input cannot keep a stream open past it.
            tokio::select! {
                biased;
                () = sleep_until(self.deadline) => {
                    if self.silent.load(Ordering::SeqCst) {
                        self.deadline = Instant::now() + self.stream_for;
                        continue;
                    }
                    // The gateway's loop waits `min(left, 15 s)` and answers a timeout with
                    // `: ping`, so a quiet stream always ends with one more ping. With events
                    // waiting (no timeout in the gateway) it just ends.
                    self.ended = true;
                    return self.rx.is_empty().then(|| b": ping\n\n".to_vec());
                }
                item = self.rx.recv() => {
                    // `None`: dropped by the hub (`drop_streams`, overflow): the stream ends.
                    let line = item?;
                    if self.silent.load(Ordering::SeqCst) {
                        continue;
                    }
                    self.next_ping = Instant::now() + self.ping_every;
                    return Some(line.into_bytes());
                }
                () = sleep_until(self.next_ping) => {
                    self.next_ping = Instant::now() + self.ping_every;
                    if !self.silent.load(Ordering::SeqCst) {
                        return Some(": ping\n\n".into());
                    }
                }
            }
        }
    }
}

impl Drop for LiveStream {
    fn drop(&mut self) {
        if let Some(s) = self.shared.upgrade() {
            let mut st = s.state.lock().unwrap();
            st.subs.retain(|x| x.id != self.sub_id);
            st.note_idle();
        }
    }
}

impl EventsHub {
    pub fn new() -> Self {
        Self::with(HubConfig::default())
    }

    pub fn with(cfg: HubConfig) -> Self {
        let st = State {
            cfg,
            // The gateway bases ids on the clock; any start works, only "always up" matters.
            last_id: 1_000_000,
            ring: VecDeque::new(),
            floor: 1_000_000,
            subs: Vec::new(),
            next_sub: 0,
            idle_since: None,
            convs: Vec::new(),
            counter: 0,
            auth: Auth::default(),
        };
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(st),
                drivers: Mutex::new(JoinSet::new()),
                opens: std::sync::atomic::AtomicUsize::new(0),
            }),
        }
    }

    /// Serves the hub from `routes` (scripted routes still win, so a test can override a path).
    pub fn install(&self, routes: &HttpRoutes) {
        routes.set_hook(Arc::new(Hook {
            shared: Arc::downgrade(&self.shared),
        }));
    }

    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared.state.lock().unwrap()
    }

    // -- test controls --------------------------------------------------------------------

    pub fn set_config(&self, f: impl FnOnce(&mut HubConfig)) {
        f(&mut self.st().cfg);
    }

    /// The scripted reply: one `token` per entry, `gap` before each.
    pub fn set_reply(&self, tokens: &[&str], gap: Duration) {
        self.set_config(|c| {
            c.reply = tokens.iter().map(|t| (*t).to_owned()).collect();
            c.token_gap = gap;
        });
    }

    /// A conversation that already exists (no events).
    pub fn seed_conversation(&self, id: &str, title: &str) {
        self.st().new_conv(id.into(), "main", Some(title));
    }

    /// History the conversation already has (no events).
    pub fn seed_history(&self, conv: &str, role: &'static str, text: &str) {
        let mut st = self.st();
        if let Some(ix) = st.conv_ix(conv) {
            st.add_history(ix, role, text);
        }
    }

    /// A history message OpenClaw gave no id (`id: null`).
    pub fn seed_history_without_id(&self, conv: &str, role: &'static str, text: &str) {
        let mut st = self.st();
        if let Some(ix) = st.conv_ix(conv) {
            st.push_history(ix, None, role, text);
        }
    }

    /// Another device writes into `conv`; returns its `message_id`.
    pub fn other_device_send(&self, conv: &str, text: &str) -> String {
        let (id, spawn) = {
            let mut st = self.st();
            let ix = st.conv_ix(conv).expect("known conversation");
            st.send(ix, ("dev-other".into(), "Cocina".into()), text)
        };
        if spawn {
            self.shared.spawn_driver(conv);
        }
        id
    }

    /// The agent speaks first (a push): `turn_start` (origin `push`, `by: null`), one `token`,
    /// `turn_end`. With `audio`, one live `audio` event for `?audio=1` streams. No `message`
    /// (GUESSED: a push has none). Not queued behind a running turn (GUESSED).
    pub fn push(&self, conv: &str, text: &str, audio: bool) {
        let mut st = self.st();
        let ix = st.conv_ix(conv).expect("known conversation");
        let turn_id = st.next_n("turn");
        let turn = Turn {
            id: turn_id.clone(),
            origin: "push",
            by: None,
            text: text.into(),
            cancel: CancellationToken::new(),
            reason: None,
        };
        let ev = st.turn_json(&turn, conv);
        st.counter += 1; // `note_turn` at the start
        let n = st.counter;
        let c = &mut st.convs[ix];
        c.turn_count += 1;
        c.talked_seq = n;
        st.publish(&ev);
        st.publish(
            &json!({"type": "token", "turn_id": turn_id, "conversation": conv, "text": text}),
        );
        if audio {
            // GUESSED payload: 3 zero bytes of PCM; the doc fixes the shape, not the size.
            st.publish_live(&json!({
                "type": "audio", "turn_id": turn_id, "conversation": conv, "data": "AAAA",
            }));
        }
        st.add_history(ix, "assistant", text);
        st.publish(&json!({
            "type": "turn_end", "turn_id": turn_id, "conversation": conv, "reason": "done",
        }));
    }

    /// Ends every open stream (the client sees a clean end; what follows is buffered).
    pub fn drop_streams(&self) {
        let mut st = self.st();
        st.subs.clear();
        st.note_idle();
    }

    /// The open streams send nothing at all (no event, no ping, no end) until they are dropped.
    /// New streams are healthy.
    pub fn go_silent(&self) {
        for s in &self.st().subs {
            s.silent.store(true, Ordering::SeqCst);
        }
    }

    /// Forgets the ring: a resume from any earlier id gets `resync`.
    pub fn forget_buffer(&self) {
        self.st().forget();
    }

    /// Every call answers `401 not_authenticated` until [`restore`](Self::restore).
    /// Like the gateway's `close_device`: the open streams are cut and the buffer is forgotten.
    pub fn revoke(&self) {
        let mut st = self.st();
        st.auth.revoked = true;
        st.subs.clear();
        st.forget();
        st.note_idle();
    }

    /// `403 missing_scope` for the calls that need `scope`.
    pub fn missing_scope(&self, scope: Scope) {
        match scope {
            Scope::Read => self.st().auth.no_read = true,
            Scope::Write => self.st().auth.no_write = true,
        }
    }

    /// Undoes `revoke` and `missing_scope`.
    pub fn restore(&self) {
        self.st().auth = Auth::default();
    }

    /// Streams currently open (the leak canary's probe).
    pub fn subscribers(&self) -> usize {
        self.st().subs.len()
    }

    /// `GET /events` requests accepted so far.
    pub fn stream_opens(&self) -> usize {
        self.shared.opens.load(Ordering::SeqCst)
    }

    /// The id of the last event (`as_of`).
    pub fn last_id(&self) -> u64 {
        self.st().last_id
    }
}

#[derive(Debug)]
struct Hook {
    shared: Weak<Shared>,
}

enum Route<'a> {
    Events,
    List,
    Agents,
    Create,
    Get(&'a str),
    Messages(&'a str),
    Post(&'a str),
    Withdraw(&'a str, &'a str),
    Cancel(&'a str),
}

impl Route<'_> {
    fn needs_write(&self) -> bool {
        matches!(
            self,
            Route::Create | Route::Post(_) | Route::Withdraw(..) | Route::Cancel(_)
        )
    }
}

fn parse_query(q: &str, key: &str) -> Option<String> {
    q.split('&')
        .find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key))
        .map(|(_, v)| v.to_owned())
}

impl RouteHook for Hook {
    fn handle(&self, req: &SeenRequest) -> Option<HookReply> {
        let shared = self.shared.upgrade()?;
        let (path, query) = req.path.split_once('?').unwrap_or((&req.path, ""));
        let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
        let route = match (req.method.as_str(), segs.as_slice()) {
            ("GET", ["events"]) => Route::Events,
            ("GET", ["conversations"]) => Route::List,
            ("GET", ["conversations", "agents"]) => Route::Agents,
            ("POST", ["conversations"]) => Route::Create,
            ("GET", ["conversations", id]) => Route::Get(id),
            ("GET", ["conversations", id, "messages"]) => Route::Messages(id),
            ("POST", ["conversations", id, "messages"]) => Route::Post(id),
            ("DELETE", ["conversations", id, "messages", m]) => Route::Withdraw(id, m),
            ("POST", ["conversations", id, "cancel"]) => Route::Cancel(id),
            _ => return None,
        };
        let mut st = shared.state.lock().unwrap();
        // Order as the gateway's dependencies: who are you (401), then the scope (403).
        let bearer = req
            .authorization
            .as_deref()
            .and_then(|a| a.strip_prefix("Bearer "))
            .is_some_and(|t| !t.trim().is_empty());
        if !bearer || st.auth.revoked {
            return Some(detail(401, "not_authenticated"));
        }
        if (route.needs_write() && st.auth.no_write) || (!route.needs_write() && st.auth.no_read) {
            return Some(detail(403, "missing_scope"));
        }
        let me = (st.cfg.device_id.clone(), st.cfg.device_name.clone());
        let mut spawn = None;
        let reply = match route {
            Route::Events => {
                shared.opens.fetch_add(1, Ordering::SeqCst);
                let audio = matches!(parse_query(query, "audio").as_deref(), Some("1" | "true"));
                HookReply::Stream(open_stream(
                    &shared,
                    &mut st,
                    req.last_event_id.as_deref(),
                    audio,
                ))
            }
            Route::List => {
                let mut order: Vec<&Conv> = st.convs.iter().collect();
                order.sort_by_key(|c| std::cmp::Reverse(c.talked_seq));
                ok(
                    200,
                    &Value::Array(order.iter().map(|c| st.conv_json(c)).collect()),
                )
            }
            Route::Agents => ok(200, &json!({"agents": ["main"], "default": "main"})),
            Route::Create => {
                let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                if !body.is_object() {
                    return Some(detail(422, "invalid_body"));
                }
                let id = st.next_n("c");
                let agent = body["agent"].as_str().unwrap_or("main").to_owned();
                let ix = st.new_conv(id.clone(), &agent, None);
                let mut v = st.conv_json(&st.convs[ix]);
                if let Some(text) = body["message"].as_str() {
                    if text.trim().is_empty() {
                        return Some(detail(422, "empty_message"));
                    }
                    let (mid, sp) = st.send(ix, me, text);
                    v["message_id"] = json!(mid);
                    spawn = sp.then_some(id);
                }
                ok(201, &v)
            }
            Route::Get(id) => match st.conv_ix(id) {
                None => detail(404, "Conversation not found"),
                Some(ix) => {
                    let c = &st.convs[ix];
                    let mut v = st.conv_json(c);
                    v["turn"] = c.turn.as_ref().map_or(Value::Null, |t| {
                        let mut j = st.turn_json(t, id);
                        j["text"] = json!(t.text);
                        j
                    });
                    v["pending"] = Value::Array(
                        c.queue
                            .iter()
                            .filter(|m| m.state == MsgState::Pending)
                            .map(|m| st.msg_json(m, id))
                            .collect(),
                    );
                    v["as_of"] = json!(st.last_id);
                    ok(200, &v)
                }
            },
            Route::Messages(id) => match st.conv_ix(id) {
                None => detail(404, "Conversation not found"),
                Some(ix) => history(&st.convs[ix], query),
            },
            Route::Post(id) => {
                let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                let Some(ix) = st.conv_ix(id) else {
                    return Some(detail(404, "Conversation not found"));
                };
                match body["text"].as_str() {
                    None => detail(422, "invalid_body"),
                    Some(t) if t.trim().is_empty() => detail(422, "empty_message"),
                    Some(t) => {
                        let (mid, sp) = st.send(ix, me, t);
                        spawn = sp.then(|| id.to_owned());
                        ok(202, &json!({ "message_id": mid }))
                    }
                }
            }
            Route::Withdraw(id, mid) => {
                let Some(ix) = st.conv_ix(id) else {
                    return Some(detail(404, "Conversation not found"));
                };
                let q = &mut st.convs[ix].queue;
                let pos = q
                    .iter()
                    .position(|m| m.id == mid && m.state == MsgState::Pending);
                match pos.and_then(|p| q.remove(p)) {
                    None => detail(404, "not_pending"),
                    Some(mut m) => {
                        m.state = MsgState::Withdrawn;
                        let ev = st.msg_json(&m, id);
                        st.publish(&ev);
                        HookReply::Plain {
                            status: 204,
                            body: Vec::new(),
                        }
                    }
                }
            }
            Route::Cancel(id) => {
                let Some(ix) = st.conv_ix(id) else {
                    return Some(detail(404, "Conversation not found"));
                };
                let c = &mut st.convs[ix];
                let cancelled = if let Some(t) = c.turn.as_mut() {
                    t.reason = Some("cancelled");
                    t.cancel.cancel();
                    true
                } else if let Some(mut m) = c.queue.pop_front() {
                    // channel.py `cancel_head`: with no turn open, the first run that has not
                    // started (pending OR delivered) is withdrawn; no turn events follow.
                    m.state = MsgState::Withdrawn;
                    let ev = st.msg_json(&m, id);
                    st.publish(&ev);
                    true
                } else {
                    false
                };
                ok(200, &json!({ "cancelled": cancelled }))
            }
        };
        drop(st);
        if let Some(c) = spawn {
            shared.spawn_driver(&c);
        }
        Some(reply)
    }
}

fn history(c: &Conv, query: &str) -> HookReply {
    let limit = parse_query(query, "limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(200)
        .clamp(1, HISTORY_MAX);
    let all: Vec<&HistMsg> = c.history.iter().collect();
    let (end, has_more_before) = match parse_query(query, "before") {
        None => (all.len(), false),
        Some(b) => match all.iter().position(|m| m.id.as_deref() == Some(b.as_str())) {
            None => return detail(404, "message_not_found"),
            Some(i) => (i, false),
        },
    };
    let start = end.saturating_sub(limit);
    let msgs: Vec<Value> = all[start..end]
        .iter()
        .map(|m| json!({"id": m.id, "role": m.role, "text": m.text, "timestamp": null}))
        .collect();
    ok(
        200,
        &json!({
            "conversation_id": c.id,
            "messages": msgs,
            "has_more": has_more_before || start > 0,
        }),
    )
}

/// `events_routes.py::stream`: registers without awaiting, so nothing is lost between the replay
/// and the queue.
fn open_stream(
    shared: &Arc<Shared>,
    st: &mut State,
    last_event_id: Option<&str>,
    audio: bool,
) -> LiveStream {
    // Garbage cannot be resumed: -1 is below any floor (resync).
    let resume = last_event_id.map(|v| v.trim().parse::<i128>().unwrap_or(-1));
    // `RESUME_S`: a device with no stream for that long is closed and loses its buffer.
    if st.subs.is_empty() && st.idle_since.is_some_and(|t| t.elapsed() > RESUME) {
        st.forget();
    }
    st.idle_since = None;
    let (tx, rx) = mpsc::channel(QUEUE_MAX);
    let silent = Arc::new(AtomicBool::new(false));
    st.next_sub += 1;
    let sub_id = st.next_sub;
    st.subs.push(Sub {
        id: sub_id,
        tx,
        audio,
        silent: silent.clone(),
    });
    let ready = json!({"type": "ready", "protocol": 5, "device_id": st.cfg.device_id});
    let mut head = VecDeque::from(["retry: 1000\n\n".to_owned()]);
    let replay = resume.filter(|r| *r >= i128::from(st.floor) && *r <= i128::from(st.last_id));
    match (resume, replay) {
        (Some(r), Some(_)) => {
            head.push_back(sse(&ready, None)); // resumed: no id
            head.extend(
                st.ring
                    .iter()
                    .filter(|(id, _)| i128::from(*id) > r)
                    .map(|(_, l)| l.clone()),
            );
        }
        (resume, _) => {
            head.push_back(sse(&ready, Some(st.last_id)));
            if resume.is_some() {
                head.push_back(sse(&json!({"type": "resync"}), Some(st.last_id)));
            }
        }
    }
    let now = Instant::now();
    LiveStream {
        shared: Arc::downgrade(shared),
        sub_id,
        head,
        rx,
        deadline: now + st.cfg.stream_for,
        ping_every: st.cfg.ping_every,
        stream_for: st.cfg.stream_for,
        next_ping: now + st.cfg.ping_every,
        silent,
        ended: false,
    }
}
