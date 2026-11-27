//! The engine task: IPC commands (typed text, cancel, takes) and `/events` events -> the
//! conversation store ([`Chat`]) and the view; REST calls to the gateway; capture and playback
//! gates. Single task, single owner of all conversation state (no locks). Every await is either
//! a cancel-safe `select!` branch or bounded by a deadline.
//!
//! S4 PR 4a (Task 4), the chat over HTTP:
//! - REST calls run in one owned `JoinSet` (`queues`: writes ≤ `REST_IN_FLIGHT`, reads one per
//!   kind), each under `Config::http_timeout`, bodies capped by the port; results come back
//!   through a `select!` branch and are dropped when stale (account epoch, open sequence).
//!   Nothing is called before the first `ready` (`Inbound::Up`).
//! - A fresh link (`Up` not resumed: first connect or account change) fetches `/auth/me` (the
//!   write scope), the list and the open conversation; `resync` fetches the list and the open
//!   conversation. A resumed link and the 25 s rotation fetch nothing (the gateway replays).
//! - `SendText`: with a conversation open `POST …/messages`; with none `POST /conversations
//!   {message}` and the new one opens. Answered once the call is admitted (`TextAccepted` =
//!   "accepted by the agent"); my text then appears from its `message` event (no local echo); a
//!   failed post shows as a failed message. `CancelTurn`: `POST …/cancel` of the open one.
//! - `403 missing_scope` on a write is remembered: later writes are refused without a call.
//!   shortcut: refused `NotReady` until IPC v6 (Task 5) adds `TextRefusal::MissingScope`.
//! - Dictation is off until PR 5: no link reports a transcriber, so the consent gate refuses
//!   every take `NoTranscriber`. Nothing plays: no path feeds the playback gate.
//!
//! S2 Decision 1: the microphone is never opened at start. The engine does not hold the
//! microphone at all: it owns the consent gate (`capture.rs`), the single point that can open a
//! take, and only on a `Command::CaptureStart` carrying the person's `UserAct`. Gateway events
//! can end a take (a dropped link) but never start one.
//!
//! S2 3c: the engine does not hold the speaker either. Output goes through the playback gate
//! (`playback.rs`), the single output point.

use crate::{
    capture::{CaptureGate, Cut, GateEvent, Owner, Readiness, Reply, Start},
    config::Config,
    conversation::{self, Chat, Effect, HISTORY_LIMIT},
    link::{Api, HttpClass, Inbound, classify_http},
    playback::PlaybackGate,
    queues,
    stats::Stats,
    time::{deadline, sleep_until_opt},
};
use fogar_ports::{
    audio::MicPermission,
    clock::{Clock, Mono},
    gateway_http::{GatewayHttp, HttpCall, HttpReply, PathId},
    ipc::{CaptureRefusal, CaptureThen, ConversationView, Notice, TextRefusal, UserActivity},
};
use fogar_proto::{
    account::{Me, Scope},
    conversations::{Conversation, ConversationState, CreateConversation, History, PostMessage},
    events::Event,
};
use secrecy::zeroize::Zeroizing;
use std::{borrow::Cow, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::{AbortHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

/// An unknown conversation in an event: the list is fetched once this much later (several
/// events of a new conversation make one call).
const LIST_DEBOUNCE: Duration = Duration::from_secs(1);
/// A failed open step is tried again after this (the conversation stays on screen as it was).
const REOPEN_AFTER: Duration = Duration::from_secs(5);

/// IPC -> engine (`queues::ENGINE_COMMANDS`). Each carries the reply channel; the IPC side waits
/// for it under `write_timeout` and drops it on expiry, and the engine then skips the command
/// (`is_closed`) instead of acting on a request the UI was told failed.
pub(crate) enum Command {
    SendText(String, oneshot::Sender<Result<(), TextRefusal>>),
    Cancel(oneshot::Sender<()>),
    /// Show this conversation (state + history, then live).
    // shortcut: no IPC request sends it until IPC v6 (S4 PR 4a Task 5); tests do.
    #[cfg_attr(not(test), allow(dead_code))]
    Open(String, oneshot::Sender<()>),
    /// The person pressed the microphone button (only `ipc.rs` can build the `Start`). `owner`
    /// closes when the connection that asked goes away.
    CaptureStart {
        start: Start,
        owner: Owner,
        reply: Reply,
    },
    CaptureEnd(CaptureThen, oneshot::Sender<()>),
    RequestMicPermission(oneshot::Sender<MicPermission>),
    /// The app shows this turn (its id as the view carries it): its audio may play. Answered
    /// `false` when the id names more than one entry of the view (ambiguous: refused).
    TurnVisible(String, oneshot::Sender<bool>),
}

/// Redacting `Debug`: the text is the user's.
impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::SendText(t, _) => write!(f, "SendText(<redacted {} bytes>)", t.len()),
            Command::Cancel(_) => f.write_str("Cancel"),
            Command::Open(id, _) => write!(f, "Open({id})"),
            Command::CaptureStart { start, .. } => write!(f, "CaptureStart({start:?})"),
            Command::CaptureEnd(then, _) => write!(f, "CaptureEnd({then:?})"),
            Command::RequestMicPermission(_) => f.write_str("RequestMicPermission"),
            Command::TurnVisible(id, _) => write!(f, "TurnVisible({} bytes)", id.len()),
        }
    }
}

pub(crate) struct Engine<C, H> {
    pub(crate) clock: Arc<C>,
    pub(crate) cfg: Arc<Config>,
    pub(crate) stats: Arc<Stats>,
    pub(crate) http: Arc<H>,
    pub(crate) in_rx: mpsc::Receiver<Inbound>,
    pub(crate) cmd_rx: mpsc::Receiver<Command>,
    pub(crate) conv: watch::Sender<ConversationView>,
    pub(crate) chat: watch::Sender<Chat>,
}

/// What a REST call was for. Holds the user's text for a write (never printed).
enum Job {
    List,
    Me,
    State { id: String, seq: u64 },
    History { id: String, seq: u64 },
    Send { text: String },
    Create { text: String },
    Cancel,
}

impl Job {
    fn is_write(&self) -> bool {
        matches!(self, Job::Send { .. } | Job::Create { .. } | Job::Cancel)
    }
}

/// A finished call: `reply` is `None` on a transport failure or a missed deadline.
struct Done {
    epoch: u64,
    job: Job,
    reply: Option<HttpReply>,
}

struct State {
    capture: CaptureGate,
    playback: PlaybackGate,
    /// The link is up (`Up` seen, no `Down`/`SignedOut` since).
    online: bool,
    /// The latest account handle (one token copy).
    api: Option<Api>,
    /// Bumped on every fresh link and sign-out: older results are dropped.
    epoch: u64,
    /// Bumped on every open: older open steps are dropped.
    open_seq: u64,
    rest: JoinSet<Done>,
    writes: usize,
    open_step: Option<AbortHandle>,
    list_in_flight: bool,
    list_again: bool,
    list_at: Option<Mono>,
    reopen_at: Option<Mono>,
    creating: bool,
    /// A write got `403 missing_scope` (or `/auth/me` lacks `conversations:write`).
    write_denied: bool,
}

fn decode<T: serde::de::DeserializeOwned>(r: &HttpReply) -> Option<T> {
    serde_json::from_slice(&r.body).ok()
}

fn min_opt(a: Option<Mono>, b: Option<Mono>) -> Option<Mono> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

impl<C: Clock, H: GatewayHttp> Engine<C, H> {
    /// `capture` is the single consent point, the only holder of the microphone; `playback`
    /// the single output point, the only holder of the speaker. Both live in the task's state
    /// (not in `Engine`, which `&self` methods share across awaits).
    pub(crate) async fn run(
        mut self,
        capture: CaptureGate,
        playback: PlaybackGate,
        ingress: CancellationToken,
    ) {
        let mut st = State {
            capture,
            playback,
            online: false,
            api: None,
            epoch: 0,
            open_seq: 0,
            rest: JoinSet::new(),
            writes: 0,
            open_step: None,
            list_in_flight: false,
            list_again: false,
            list_at: None,
            reopen_at: None,
            creating: false,
            write_denied: false,
        };
        self.refresh_mic(&st);
        // No IPC (or it stopped): the closed channel must not turn the branch into a hot loop.
        let mut cmds_open = true;
        loop {
            let wake_at = min_opt(st.list_at, st.reopen_at);
            tokio::select! {
                biased;
                () = ingress.cancelled() => break,
                // Review M3: the take before the gateway. A gateway that keeps `in_rx` full must
                // never starve the take's cuts (safety, silence, owner gone) or its device jobs.
                ev = st.capture.next(&*self.clock) => self.on_capture(&mut st, ev),
                // The output's own timers and device jobs are not starved by a busy gateway.
                () = st.playback.next(&*self.clock) => {}
                // An empty set resolves `None` at once: only polled while calls run.
                Some(done) = st.rest.join_next(), if !st.rest.is_empty() => {
                    if let Ok(done) = done {
                        self.on_done(&mut st, done);
                    }
                }
                ev = self.in_rx.recv() => match ev {
                    None => break,
                    Some(ev) => self.on_inbound(&mut st, ev),
                },
                cmd = self.cmd_rx.recv(), if cmds_open => match cmd {
                    None => cmds_open = false,
                    Some(cmd) => self.on_command(&mut st, cmd),
                },
                () = sleep_until_opt(&*self.clock, wake_at), if wake_at.is_some() => {
                    self.on_timer(&mut st);
                }
            }
        }
        // Agent stop (`ingress`): calls in flight are dropped, the take closes, the output goes
        // silent, and both devices are released off the runtime.
        st.rest.abort_all();
        let clock = &*self.clock;
        tokio::join!(st.capture.shutdown(clock), st.playback.shutdown(clock));
    }

    /// Applies `f` to the store; when it says it changed, publishes the store and the view.
    fn store<R: Default>(&self, f: impl FnOnce(&mut Chat) -> (R, bool)) -> R {
        let mut out = R::default();
        let changed = self.chat.send_if_modified(|c| {
            let (r, changed) = f(c);
            out = r;
            changed
        });
        if changed {
            let chat = self.chat.borrow();
            self.conv.send_if_modified(|v| chat.render(v));
        }
        out
    }

    fn publish_activity(&self, st: &State) {
        let a = if st.capture.is_active() {
            UserActivity::Recording
        } else {
            UserActivity::Idle
        };
        self.conv
            .send_if_modified(|v| conversation::set_activity(v, a));
    }

    /// The permission STATUS only (the port guarantees it never prompts).
    fn refresh_mic(&self, st: &State) {
        let m = st.capture.permission();
        self.conv.send_if_modified(|v| conversation::set_mic(v, m));
    }

    /// Starts one REST call. `false` = not started (offline, or writes at their cap).
    fn call(&self, st: &mut State, job: Job, call: HttpCall, body: Option<Vec<u8>>) -> bool {
        let Some(api) = st.api.clone().filter(|_| st.online) else {
            return false;
        };
        if job.is_write() {
            if st.writes >= queues::REST_IN_FLIGHT {
                Stats::inc(&self.stats.rest_busy);
                return false;
            }
            st.writes += 1;
        }
        Stats::inc(&self.stats.rest_calls);
        let (clock, http, d, epoch) = (
            self.clock.clone(),
            self.http.clone(),
            self.cfg.http_timeout,
            st.epoch,
        );
        let open = matches!(job, Job::State { .. } | Job::History { .. });
        let handle = st.rest.spawn(async move {
            let req = http.request(&api.addr, &call, Some(&api.token), body.map(Zeroizing::new));
            let reply = deadline(&*clock, d, req).await.and_then(Result::ok);
            Done { epoch, job, reply }
        });
        if open && let Some(old) = st.open_step.replace(handle) {
            old.abort();
        }
        true
    }

    fn refresh_list(&self, st: &mut State) {
        st.list_at = None;
        if st.list_in_flight {
            st.list_again = true;
            return;
        }
        Stats::inc(&self.stats.list_refreshes);
        st.list_in_flight = self.call(st, Job::List, HttpCall::ListConversations, None);
    }

    /// (Re)opens `id`: its state is asked first.
    fn open(&self, st: &mut State, id: &str) {
        st.reopen_at = None;
        st.open_seq += 1;
        self.store(|c| ((), c.open(id)));
        let Some(path) = PathId::new(id) else {
            // Not an id the gateway could have given: nothing to show.
            self.store(|c| ((), c.close()));
            return;
        };
        let job = Job::State {
            id: id.to_owned(),
            seq: st.open_seq,
        };
        if !self.call(st, job, HttpCall::GetConversation { id: path }, None) {
            self.retry_open(st);
        }
    }

    /// Tries the open again later; offline, the next `Up` does it (no timer while offline).
    fn retry_open(&self, st: &mut State) {
        if st.online {
            st.reopen_at = Some(self.clock.now().saturating_add(REOPEN_AFTER));
        }
    }

    fn reopen(&self, st: &mut State) {
        let id = self.chat.borrow().open_id().map(str::to_owned);
        if let Some(id) = id {
            self.open(st, &id);
        }
    }

    fn on_timer(&self, st: &mut State) {
        let now = self.clock.now();
        if st.list_at.is_some_and(|t| t.0 <= now.0) {
            self.refresh_list(st);
        }
        if st.reopen_at.is_some_and(|t| t.0 <= now.0) {
            self.reopen(st);
        }
    }

    fn effect(&self, st: &mut State, fx: Effect) {
        if fx.refresh_list && st.list_at.is_none() {
            st.list_at = Some(self.clock.now().saturating_add(LIST_DEBOUNCE));
        }
        if fx.reopen {
            Stats::inc(&self.stats.open_overflows);
            self.reopen(st);
        }
    }

    fn on_command(&self, st: &mut State, cmd: Command) {
        match cmd {
            Command::SendText(text, reply) => {
                // The IPC gave up (and counted it as busy): never act on it late.
                if reply.is_closed() {
                    return;
                }
                // Also while the device is opening (review B4): the take is already the person's.
                if st.capture.is_active() {
                    let _ = reply.send(Err(TextRefusal::Recording));
                    return;
                }
                // shortcut: `TextRefusal::MissingScope` arrives with IPC v6 (Task 5).
                if !st.online || st.write_denied || st.creating {
                    let _ = reply.send(Err(TextRefusal::NotReady));
                    return;
                }
                let open = self.chat.borrow().open_id().map(str::to_owned);
                let started = match open {
                    Some(id) => {
                        let body = serde_json::to_vec(&PostMessage {
                            text: Cow::Borrowed(&text),
                            interrupt: None,
                        });
                        match (PathId::new(&id), body) {
                            (Some(id), Ok(body)) => self.call(
                                st,
                                Job::Send { text },
                                HttpCall::PostMessage { id },
                                Some(body),
                            ),
                            _ => false,
                        }
                    }
                    None => {
                        let body = serde_json::to_vec(&CreateConversation {
                            agent: self.cfg.agent.as_deref().map(Cow::Borrowed),
                            message: Some(Cow::Borrowed(&text)),
                        });
                        let ok = body.is_ok_and(|b| {
                            self.call(
                                st,
                                Job::Create { text },
                                HttpCall::CreateConversation,
                                Some(b),
                            )
                        });
                        if ok {
                            st.creating = true;
                            self.store(|c| {
                                c.begin_create();
                                ((), false)
                            });
                        }
                        ok
                    }
                };
                if started {
                    Stats::inc(&self.stats.texts_sent);
                    let _ = reply.send(Ok(()));
                }
                // Not started (writes at their cap): the dropped reply is "busy" for the IPC.
            }
            Command::Cancel(reply) => {
                if reply.is_closed() {
                    return;
                }
                let open = self.chat.borrow().open_id().and_then(PathId::new);
                match open {
                    Some(id) if st.online && !st.write_denied => {
                        if self.call(st, Job::Cancel, HttpCall::Cancel { id }, None) {
                            Stats::inc(&self.stats.cancels_sent);
                            let _ = reply.send(());
                        }
                    }
                    // Nothing to cancel (or no way to): handled.
                    _ => {
                        let _ = reply.send(());
                    }
                }
            }
            Command::Open(id, reply) => {
                if reply.is_closed() {
                    return;
                }
                self.open(st, &id);
                let _ = reply.send(());
            }
            Command::CaptureStart {
                start,
                owner,
                reply,
            } => {
                // The IPC gave up (and counted it as busy): never open the microphone late.
                if reply.is_closed() {
                    return;
                }
                let ready = Readiness {
                    link_ready: st.online,
                    transcriber: self.conv.borrow().transcriber,
                };
                st.capture
                    .start(start, ready, owner, reply, self.clock.now());
                // A refusal may come from a permission changed in System Settings.
                self.refresh_mic(st);
            }
            Command::CaptureEnd(then, reply) => {
                // Ending is always safe: done even if the IPC already gave up waiting.
                let _ = reply.send(());
                self.end_take(st, then, None);
            }
            Command::RequestMicPermission(reply) => {
                if !reply.is_closed() {
                    st.capture.ask_permission(reply);
                }
            }
            Command::TurnVisible(id, reply) => {
                // The IPC gave up (and counted it as busy): never act on it late.
                if reply.is_closed() {
                    return;
                }
                // Review F2 of #32: the confirmation must name ONE entry of the view without
                // ambiguity (ids are compared as the view shows them, truncated included).
                let entries = self
                    .conv
                    .borrow()
                    .turns
                    .iter()
                    .filter(|t| !id.is_empty() && t.turn_id == id)
                    .count();
                if entries > 1 {
                    Stats::inc(&self.stats.turn_visible_refused);
                    let _ = reply.send(false);
                    return;
                }
                let _ = reply.send(true);
                st.playback.visible(&id, self.clock.now());
            }
        }
    }

    fn on_capture(&self, st: &mut State, ev: GateEvent) {
        match ev {
            GateEvent::Nothing => {}
            GateEvent::Refused { reply, why } => {
                let _ = reply.send(Err(why));
                self.refresh_mic(st);
            }
            GateEvent::Opened { reply } => {
                // Unreachable while no link reports a transcriber (the gate refuses first);
                // kept so a take that did open is closed, never left on.
                let now = self.clock.now();
                st.capture.close(now, CaptureRefusal::NotReady);
                let _ = reply.send(Err(CaptureRefusal::NoTranscriber));
                self.refresh_mic(st);
            }
            GateEvent::Chunk(_) => {
                // No uplink in 4a: nothing to stream to.
                Stats::inc(&self.stats.mic_chunks);
            }
            GateEvent::Cut(cut) => {
                match cut {
                    Cut::Silence => Stats::inc(&self.stats.captures_silence_cut),
                    Cut::Safety => Stats::inc(&self.stats.captures_safety_cut),
                    Cut::OwnerGone | Cut::DeviceEnded => {}
                }
                self.end_take(st, cut.then(), cut.notice());
            }
            GateEvent::Permission(p, waiters) => {
                for w in waiters {
                    let _ = w.send(p);
                }
                self.refresh_mic(st);
            }
        }
    }

    /// Closes the take (if any): the device is released off the runtime. A take shorter than
    /// `capture_min` is discarded whatever was asked; nothing is ever sent (no dictation in 4a).
    fn end_take(&self, st: &mut State, then: CaptureThen, notice: Option<Notice>) {
        let now = self.clock.now();
        let Some(closed) = st.capture.close(now, CaptureRefusal::Busy) else {
            return;
        };
        if then == CaptureThen::Discard || closed.duration < self.cfg.capture_min {
            Stats::inc(&self.stats.captures_discarded);
        }
        self.conv
            .send_if_modified(|v| conversation::take_ended(v, notice));
        self.publish_activity(st);
    }

    fn on_inbound(&self, st: &mut State, ev: Inbound) {
        match ev {
            Inbound::Up { api, resumed } => {
                st.online = true;
                st.api = Some(api);
                self.refresh_mic(st);
                if !resumed {
                    // A fresh stream (first connect, new account, lost place): fetch all once.
                    st.epoch += 1;
                    st.list_in_flight = false;
                    st.creating = false;
                    st.write_denied = false;
                    self.store(|c| {
                        c.create_failed();
                        ((), false)
                    });
                    self.call(st, Job::Me, HttpCall::Me, None);
                    self.refresh_list(st);
                    self.reopen(st);
                } else if st.open_step.is_none()
                    && self
                        .chat
                        .borrow()
                        .open
                        .as_ref()
                        .is_some_and(|o| !o.is_live())
                {
                    // An open that failed while offline (no retry timer then) goes on now.
                    self.reopen(st);
                }
            }
            Inbound::Down => {
                // The open turns stay: a resumed stream replays what was missed (or `resync`).
                st.online = false;
                self.close_take(st);
            }
            Inbound::SignedOut => {
                st.online = false;
                st.api = None;
                st.epoch += 1;
                st.rest.abort_all();
                st.writes = 0;
                st.open_step = None;
                st.list_in_flight = false;
                st.list_again = false;
                st.list_at = None;
                st.reopen_at = None;
                st.creating = false;
                st.write_denied = false;
                self.close_take(st);
                self.store(|c| ((), c.clear()));
            }
            Inbound::Resync => {
                self.refresh_list(st);
                self.reopen(st);
            }
            Inbound::Event { ev, id, charge } => {
                match &ev {
                    Event::TurnStart(_) => Stats::inc(&self.stats.turns_started),
                    Event::TurnEnd(_) => Stats::inc(&self.stats.turns_completed),
                    _ => {}
                }
                let fx = self.store(|c| {
                    let fx = c.on_event(id, ev);
                    (fx, fx.view || fx.list)
                });
                // The store copied what it keeps: the queued bytes are released.
                drop(charge);
                self.effect(st, fx);
            }
        }
    }

    /// The link that heard the take is gone: close it.
    fn close_take(&self, st: &mut State) {
        if st
            .capture
            .close(self.clock.now(), CaptureRefusal::NotReady)
            .is_some()
        {
            self.conv
                .send_if_modified(|v| conversation::take_ended(v, None));
            self.publish_activity(st);
        }
        // Silence, and release the output device while offline (energy).
        st.playback.link_down(self.clock.now());
    }

    fn on_done(&self, st: &mut State, done: Done) {
        let Done { epoch, job, reply } = done;
        if job.is_write() {
            st.writes = st.writes.saturating_sub(1);
        }
        if epoch != st.epoch {
            return;
        }
        let class = reply.as_ref().map_or(HttpClass::Retry, |r| {
            classify_http(r.status, Some(r.body.as_slice()))
        });
        let ok = class == HttpClass::Ok;
        if !ok {
            Stats::inc(&self.stats.rest_failures);
        }
        if class == HttpClass::MissingScope && job.is_write() {
            Stats::inc(&self.stats.writes_missing_scope);
            st.write_denied = true;
        }
        let reply = reply.filter(|_| ok);
        match job {
            Job::List => {
                st.list_in_flight = false;
                if let Some(list) = reply.as_ref().and_then(decode::<Vec<Conversation>>) {
                    self.store(|c| ((), c.set_list(&list)));
                }
                if std::mem::take(&mut st.list_again) {
                    self.refresh_list(st);
                }
            }
            Job::Me => {
                // Unknown scopes (an old gateway, an odd body) deny nothing.
                if let Some(me) = reply.as_ref().and_then(decode::<Me>)
                    && !me.scopes.is_empty()
                {
                    st.write_denied = !me.scopes.contains(&Scope::ConversationsWrite);
                }
            }
            Job::State { id, seq } if seq == st.open_seq => {
                st.open_step = None;
                match reply.as_ref().and_then(decode::<ConversationState>) {
                    Some(state) => {
                        // Not a view change yet (the history comes next); the list may mark it.
                        if !self.store(|c| {
                            let ok = c.on_state(&id, state);
                            (ok, ok)
                        }) {
                            return;
                        }
                        let path = PathId::new(&id);
                        let next = Job::History { id, seq };
                        let started = path.is_some_and(|id| {
                            self.call(
                                st,
                                next,
                                HttpCall::GetMessages {
                                    id,
                                    before: None,
                                    limit: HISTORY_LIMIT,
                                },
                                None,
                            )
                        });
                        if !started {
                            self.retry_open(st);
                        }
                    }
                    None => self.open_failed(st, class),
                }
            }
            Job::History { id, seq } if seq == st.open_seq => {
                st.open_step = None;
                match reply.as_ref().and_then(decode::<History>) {
                    Some(h) => {
                        self.store(|c| {
                            let changed = c.on_history(&id, h);
                            (changed, changed)
                        });
                    }
                    None => self.open_failed(st, class),
                }
            }
            // A newer open replaced it.
            Job::State { .. } | Job::History { .. } => {}
            Job::Send { text } => {
                if reply.is_none() {
                    self.store(|c| {
                        let changed = c.send_failed(&text, failure_code(class));
                        (changed, changed)
                    });
                }
            }
            Job::Create { text } => {
                st.creating = false;
                match reply.as_ref().and_then(decode::<Conversation>) {
                    Some(conv) => {
                        let fx = self.store(|c| (c.created(&conv), true));
                        st.open_seq += 1;
                        self.effect(st, fx);
                    }
                    None => {
                        self.store(|c| {
                            c.create_failed();
                            ((), false)
                        });
                        // shortcut: with nothing open the failed text is not shown (IPC v6
                        // carries the refusal, Task 5); counted in `rest_failures`.
                        drop(text);
                    }
                }
            }
            Job::Cancel => {}
        }
    }

    /// An open step failed: a gone conversation closes, anything else is tried again later.
    fn open_failed(&self, st: &mut State, class: HttpClass) {
        if class == HttpClass::Fatal {
            self.store(|c| ((), c.close()));
            self.refresh_list(st);
        } else {
            self.retry_open(st);
        }
    }
}

/// The code a failed post shows with.
fn failure_code(class: HttpClass) -> &'static str {
    match class {
        HttpClass::MissingScope => "missing_scope",
        HttpClass::Revoked => "not_authenticated",
        HttpClass::Fatal => "refused",
        HttpClass::Ok | HttpClass::Retry => "not_sent",
    }
}

#[cfg(test)]
mod tests;
