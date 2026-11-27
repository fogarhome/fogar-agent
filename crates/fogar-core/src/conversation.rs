//! The conversations as the agent knows them (S4 PR 4a): the [`Chat`] store, owned by the engine
//! task, and its projection into the bounded [`ConversationView`] the UI draws (spec S2,
//! Decision 3). Pure: every function mutates in place and says what changed, so the engine can
//! drive `watch::Sender::send_if_modified` without cloning.
//!
//! - **List** (`GET /conversations`): at most [`LIST_MAX`] items in the gateway's order. A
//!   `turn_start` of any conversation marks it running and moves it to the top; its `turn_end`
//!   clears the mark (Sito, 2026-11-27). `GET /conversations` has no running flag: the marks live
//!   in their own bounded set and survive a refresh. An event of a conversation the list does not
//!   have asks for a refresh ([`Effect::refresh_list`]).
//! - **The open conversation**: `GET /conversations/{id}` (state + `as_of`), then
//!   `GET …/messages?limit=50` (history). Its events arriving meanwhile wait in a bounded
//!   [`Buffer`] (overflow: open again, [`Effect::reopen`]); after that only events with an SSE id
//!   `> as_of` apply. History ids are OpenClaw's and live `message` ids are run ids: they are
//!   never compared (gateway facts at 290efa0).
//! - **A turn finishing during the open** (GUESSED: the gateway promises no order between
//!   `turn_end` and `chat.history`): a turn whose `turn_end` arrived before the history reply is
//!   taken from history and its live copy (the state's `turn` or buffered events) is dropped; a
//!   turn that ends later stays live (the history fetched earlier lacks its reply). Likewise
//!   (GUESSED, one run at a time) a turn that STARTED before the history reply has its user
//!   message in history: the last delivered message before that `turn_start` is dropped.
//! - **Creating with a message**: the gateway publishes the `message` before its `201`, so while
//!   a create is in flight the events of conversations nobody knows are kept (same bounds); the
//!   new conversation then opens live from its first event, without a re-fetch (it has no
//!   history yet). If that buffer overflowed it is opened like any other.
//! - Other conversations' events never keep text. A withdrawn message disappears (Sito).
//!
//! Bounds: list ≤ [`LIST_MAX`]; open entries ≤ [`MAX_ENTRIES`] and ≤ `CONVERSATION_MAX_TEXT_BYTES`
//! of text (oldest first); buffers ≤ [`BUFFER_EVENTS`] / [`BUFFER_BYTES`]; gateway ids, codes and
//! names cut to [`ID_MAX_BYTES`], titles to [`TITLE_MAX_BYTES`]. `Debug` prints sizes, never text,
//! titles or names.

use fogar_ports::{
    audio::MicPermission,
    ipc::{
        CONVERSATION_MAX_TEXT_BYTES, CONVERSATION_MAX_TURNS, ConversationView, Notice,
        ProgressKind, ServiceState, TurnAudio, TurnOutcome, TurnSource, TurnView,
    },
};
use fogar_proto::{
    conversations::{Conversation, ConversationState, History, Role},
    events::{Event, MessageEv, MsgState, TurnStartEv},
    server::{ProgressPhase, TurnEndReason, TurnOrigin},
};
use std::collections::VecDeque;

/// Longest gateway id, code or device name kept.
pub const ID_MAX_BYTES: usize = 128;
/// Longest conversation title kept in the list.
pub const TITLE_MAX_BYTES: usize = 256;
/// Conversations kept in the list.
pub const LIST_MAX: usize = 50;
/// Entries (messages + turns) of the open conversation: twenty exchanges.
pub const MAX_ENTRIES: usize = 2 * CONVERSATION_MAX_TURNS;
/// Events held while a conversation opens (or a create is in flight).
pub const BUFFER_EVENTS: usize = 256;
/// ... and their bytes (text + a fixed overhead per event).
pub const BUFFER_BYTES: usize = 64 * 1024;
/// History asked for when opening (PR 3 carry-over).
pub const HISTORY_LIMIT: u16 = 50;

/// `s` cut to [`ID_MAX_BYTES`] at a char boundary.
fn bounded(s: &str) -> &str {
    cut(s, ID_MAX_BYTES)
}

/// A gateway id as the view shows it (and as the app sends it back in `TurnVisible`).
pub(crate) fn bounded_id(s: &str) -> &str {
    bounded(s)
}

/// `s` cut to `max` bytes at a char boundary.
fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut n = max;
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

fn truncate(s: &mut String, max: usize) {
    let n = cut(s, max).len();
    s.truncate(n);
}

fn set<T: PartialEq>(slot: &mut T, v: T) -> bool {
    let changed = *slot != v;
    *slot = v;
    changed
}

// ---- the store --------------------------------------------------------------------------

/// One conversation of the list. `Debug` hides the title (OpenClaw derives it from user text).
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ConvItem {
    pub id: String,
    pub title: Option<String>,
    /// Private to one person (`owner_user_id`), else the home's.
    pub private: bool,
    pub last_turn_at: Option<String>,
    /// A turn runs in it now (any device).
    pub running: bool,
}

impl std::fmt::Debug for ConvItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConvItem")
            .field("id", &self.id)
            .field("title_bytes", &self.title.as_ref().map(String::len))
            .field("private", &self.private)
            .field("running", &self.running)
            .finish_non_exhaustive()
    }
}

/// Where a message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgStatus {
    /// Waiting behind a running turn.
    Pending,
    /// Reached the agent.
    Delivered,
    /// From history.
    Past,
    /// Our `POST` failed (`MsgEntry::error_code` says why); it never reached the gateway.
    Failed,
}

/// A person's message. `id` is the run id of a live message, empty for history and failures.
#[derive(Clone, PartialEq, Eq)]
pub struct MsgEntry {
    pub id: String,
    pub text: String,
    /// Sent by this device.
    pub mine: bool,
    /// The sending device's name (another device's message).
    pub from_name: Option<String>,
    pub status: MsgStatus,
    pub error_code: Option<String>,
}

impl std::fmt::Debug for MsgEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MsgEntry")
            .field("id", &self.id)
            .field("text_bytes", &self.text.len())
            .field("mine", &self.mine)
            .field("from_name_bytes", &self.from_name.as_ref().map(String::len))
            .field("status", &self.status)
            .field("error_code", &self.error_code)
            .finish()
    }
}

/// A turn of the agent. `turn_id` is empty for history.
#[derive(Clone, PartialEq, Eq)]
pub struct TurnEntry {
    pub turn_id: String,
    /// Started by a push (the agent spoke first), else by a person's message.
    pub push: bool,
    /// Who started it (a person's turn), as `turn_start.by` names them.
    pub by_name: Option<String>,
    pub by_self: bool,
    pub reply: String,
    pub progress: Option<ProgressKind>,
    pub tool: Option<String>,
    /// `None` while it runs.
    pub outcome: Option<TurnOutcome>,
    pub error_code: Option<String>,
}

impl std::fmt::Debug for TurnEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnEntry")
            .field("turn_id", &self.turn_id)
            .field("push", &self.push)
            .field("by_name_bytes", &self.by_name.as_ref().map(String::len))
            .field("by_self", &self.by_self)
            .field("reply_bytes", &self.reply.len())
            .field("progress", &self.progress)
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Msg(MsgEntry),
    Turn(TurnEntry),
}

impl Entry {
    fn text_len(&self) -> usize {
        match self {
            Entry::Msg(m) => m.text.len(),
            Entry::Turn(t) => t.reply.len(),
        }
    }
    fn text_mut(&mut self) -> &mut String {
        match self {
            Entry::Msg(m) => &mut m.text,
            Entry::Turn(t) => &mut t.reply,
        }
    }
}

/// Events held while their conversation opens. Overflow drops them all (memory is freed) and
/// the conversation is opened again.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Buffer {
    evs: Vec<(Option<u64>, Event)>,
    bytes: usize,
    overflowed: bool,
}

impl std::fmt::Debug for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Buffer")
            .field("events", &self.evs.len())
            .field("bytes", &self.bytes)
            .field("overflowed", &self.overflowed)
            .finish()
    }
}

/// Bytes an event holds (its texts plus a fixed overhead).
fn ev_bytes(ev: &Event) -> usize {
    64 + match ev {
        Event::Message(m) => m.text.len() + m.from.name.as_ref().map_or(0, String::len),
        Event::Token(t) => t.text.len(),
        Event::TurnStart(t) => t.text.as_ref().map_or(0, String::len),
        _ => 0,
    }
}

impl Buffer {
    /// `true` when this push overflowed it (the first time only).
    fn push(&mut self, id: Option<u64>, ev: Event) -> bool {
        if self.overflowed {
            return false;
        }
        let n = ev_bytes(&ev);
        if self.evs.len() >= BUFFER_EVENTS || self.bytes + n > BUFFER_BYTES {
            self.evs = Vec::new();
            self.bytes = 0;
            self.overflowed = true;
            return true;
        }
        self.bytes += n;
        self.evs.push((id, ev));
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// `GET /conversations/{id}` in flight.
    State { buf: Buffer },
    /// `GET …/messages` in flight; the state arrived.
    History {
        as_of: u64,
        turn: Option<TurnEntry>,
        pending: Vec<MsgEntry>,
        buf: Buffer,
    },
    /// Events apply as they come (those with an id `<= as_of` are already included).
    Live { as_of: u64 },
}

/// The conversation on screen.
#[derive(Clone, PartialEq, Eq)]
pub struct OpenConv {
    pub id: String,
    /// Oldest first.
    pub entries: Vec<Entry>,
    phase: Phase,
}

impl std::fmt::Debug for OpenConv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenConv")
            .field("id", &self.id)
            .field("entries", &self.entries)
            .field("phase", &self.phase)
            .finish()
    }
}

impl OpenConv {
    /// The state and history arrived: events apply live.
    pub fn is_live(&self) -> bool {
        matches!(self.phase, Phase::Live { .. })
    }
}

/// What an update asks of the engine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Effect {
    /// The open conversation changed (publish the view).
    pub view: bool,
    /// The list changed.
    pub list: bool,
    /// An event named a conversation the list does not have.
    pub refresh_list: bool,
    /// The open buffer overflowed: open the conversation again.
    pub reopen: bool,
}

/// The store: the list, the open conversation, and what waits for a create.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Chat {
    pub list: Vec<ConvItem>,
    pub open: Option<OpenConv>,
    /// Conversations with a turn running, newest last (≤ `LIST_MAX`).
    running: VecDeque<String>,
    /// Events of unknown conversations while a `POST /conversations` is in flight.
    creating: Option<Buffer>,
}

fn conv_of(ev: &Event) -> Option<&str> {
    match ev {
        Event::Message(m) => m.conversation.as_deref(),
        Event::TurnStart(t) => t.conversation.as_deref(),
        Event::Token(t) => t.conversation.as_deref(),
        Event::TurnProgress(p) => p.conversation.as_deref(),
        Event::Error(e) => e.conversation.as_deref(),
        Event::TurnEnd(t) => t.conversation.as_deref(),
        Event::Ready { .. } | Event::Resync | Event::ToolCall | Event::Audio | Event::Unknown => {
            None
        }
    }
}

fn turn_id_of(ev: &Event) -> Option<&str> {
    match ev {
        Event::TurnStart(t) => Some(&t.turn_id),
        Event::Token(t) => Some(&t.turn_id),
        Event::TurnProgress(p) => Some(&p.turn_id),
        Event::Error(e) => e.turn_id.as_deref(),
        Event::TurnEnd(t) => Some(&t.turn_id),
        _ => None,
    }
}

fn msg_entry(m: &MessageEv) -> MsgEntry {
    MsgEntry {
        id: bounded(&m.id).to_owned(),
        text: m.text.clone(),
        mine: m.from.self_,
        from_name: m.from.name.as_deref().map(|n| bounded(n).to_owned()),
        status: match m.state {
            MsgState::Pending => MsgStatus::Pending,
            _ => MsgStatus::Delivered,
        },
        error_code: None,
    }
}

fn turn_entry(t: &TurnStartEv) -> TurnEntry {
    TurnEntry {
        turn_id: bounded(&t.turn_id).to_owned(),
        push: t.origin == Some(TurnOrigin::Push),
        by_name: t.by.as_ref().map(|b| bounded(&b.name).to_owned()),
        by_self: t.by.as_ref().is_some_and(|b| b.self_),
        reply: t.text.clone().unwrap_or_default(),
        progress: None,
        tool: None,
        outcome: None,
        error_code: None,
    }
}

fn outcome(r: Option<TurnEndReason>) -> TurnOutcome {
    match r {
        None | Some(TurnEndReason::Done) => TurnOutcome::Done,
        Some(TurnEndReason::Error) => TurnOutcome::Error,
        Some(TurnEndReason::Cancelled) => TurnOutcome::Cancelled,
        Some(TurnEndReason::Interrupted) => TurnOutcome::Interrupted,
        Some(TurnEndReason::Replaced) => TurnOutcome::Replaced,
        Some(TurnEndReason::Timeout) => TurnOutcome::Timeout,
        Some(TurnEndReason::Closed) => TurnOutcome::Closed,
    }
}

fn progress(p: ProgressPhase) -> ProgressKind {
    match p {
        ProgressPhase::Preparing => ProgressKind::Preparing,
        ProgressPhase::Thinking => ProgressKind::Thinking,
        ProgressPhase::Tool => ProgressKind::Tool,
        ProgressPhase::Working => ProgressKind::Working,
    }
}

fn text_bytes(entries: &[Entry]) -> usize {
    entries.iter().map(Entry::text_len).sum()
}

/// Drops the oldest entries until both bounds hold; a single entry over the text bound is cut.
fn trim(entries: &mut Vec<Entry>) {
    let excess = entries.len().saturating_sub(MAX_ENTRIES);
    entries.drain(..excess);
    let mut total = text_bytes(entries);
    let mut drop = 0;
    while entries.len() - drop > 1 && total > CONVERSATION_MAX_TEXT_BYTES {
        total -= entries[drop].text_len();
        drop += 1;
    }
    entries.drain(..drop);
    if let Some(e) = entries.first_mut() {
        truncate(e.text_mut(), CONVERSATION_MAX_TEXT_BYTES);
    }
}

/// The running turn with this id; never matches an empty id.
fn open_turn<'a>(entries: &'a mut [Entry], id: &str) -> Option<&'a mut TurnEntry> {
    if id.is_empty() {
        return None;
    }
    let id = bounded(id);
    entries.iter_mut().rev().find_map(|e| match e {
        Entry::Turn(t) if t.turn_id == id && t.outcome.is_none() => Some(t),
        _ => None,
    })
}

fn end(t: &mut TurnEntry, o: TurnOutcome) {
    t.outcome = Some(o);
    t.progress = None;
    t.tool = None;
}

/// Applies one live event of the open conversation. `true` = changed.
fn apply(entries: &mut Vec<Entry>, ev: &Event) -> bool {
    let changed = match ev {
        Event::Message(m) => {
            let id = bounded(&m.id);
            let at = (!id.is_empty())
                .then(|| {
                    entries
                        .iter()
                        .position(|e| matches!(e, Entry::Msg(x) if x.id == id))
                })
                .flatten();
            match (at, m.state) {
                (Some(i), MsgState::Withdrawn) => {
                    entries.remove(i);
                    true
                }
                (None, MsgState::Withdrawn) => false,
                (Some(i), state) => match &mut entries[i] {
                    Entry::Msg(x) if state == MsgState::Pending => {
                        set(&mut x.status, MsgStatus::Pending)
                    }
                    Entry::Msg(x) if state == MsgState::Delivered => {
                        set(&mut x.status, MsgStatus::Delivered)
                    }
                    _ => false,
                },
                (None, _) => {
                    entries.push(Entry::Msg(msg_entry(m)));
                    true
                }
            }
        }
        Event::TurnStart(ts) => {
            let id = bounded(&ts.turn_id);
            let known = entries
                .iter()
                .any(|e| matches!(e, Entry::Turn(t) if !id.is_empty() && t.turn_id == id));
            if known {
                return false;
            }
            // One turn runs at a time: one left without its turn_end was replaced.
            for e in entries.iter_mut() {
                if let Entry::Turn(t) = e
                    && t.outcome.is_none()
                    && !t.turn_id.is_empty()
                {
                    end(t, TurnOutcome::Replaced);
                }
            }
            entries.push(Entry::Turn(turn_entry(ts)));
            true
        }
        Event::Token(tok) => {
            if open_turn(entries, &tok.turn_id).is_none() {
                if tok.turn_id.is_empty() {
                    return false;
                }
                // Joined mid-turn (the state came before its turn_start was kept).
                let mut t = turn_entry(&TurnStartEv {
                    turn_id: tok.turn_id.clone(),
                    conversation: None,
                    origin: None,
                    by: None,
                    text: None,
                });
                t.push = false;
                entries.push(Entry::Turn(t));
            }
            match open_turn(entries, &tok.turn_id) {
                Some(t) => {
                    t.reply.push_str(&tok.text);
                    t.progress = None;
                    t.tool = None;
                    true
                }
                None => false,
            }
        }
        Event::TurnProgress(p) => match open_turn(entries, &p.turn_id) {
            Some(t) => {
                set(&mut t.progress, Some(progress(p.phase))) | set(&mut t.tool, p.tool.clone())
            }
            None => false,
        },
        Event::TurnEnd(te) => match open_turn(entries, &te.turn_id) {
            Some(t) => {
                end(t, outcome(te.reason));
                true
            }
            None => false,
        },
        Event::Error(e) => match e.turn_id.as_deref().and_then(|id| open_turn(entries, id)) {
            Some(t) => {
                end(t, TurnOutcome::Error);
                // Over `/events` the code is always TURN_ERROR with the reason in `message`.
                let code = if e.message.is_empty() {
                    &e.code
                } else {
                    &e.message
                };
                t.error_code = Some(bounded(code).to_owned());
                true
            }
            None => false,
        },
        _ => false,
    };
    if changed {
        trim(entries);
    }
    changed
}

impl Chat {
    /// The id of the open conversation.
    pub fn open_id(&self) -> Option<&str> {
        self.open.as_ref().map(|o| o.id.as_str())
    }

    /// Starts opening `id` (its state is asked next). What was shown goes.
    pub fn open(&mut self, id: &str) -> bool {
        self.open = Some(OpenConv {
            id: bounded(id).to_owned(),
            entries: Vec::new(),
            phase: Phase::State {
                buf: Buffer::default(),
            },
        });
        true
    }

    /// Nothing open any more (signed out, or the conversation is gone).
    pub fn close(&mut self) -> bool {
        self.open.take().is_some()
    }

    /// Signed out: forget everything.
    pub fn clear(&mut self) -> bool {
        let changed = !self.list.is_empty() || self.open.is_some();
        *self = Chat::default();
        changed
    }

    fn set_running(&mut self, id: &str, on: bool) -> bool {
        let id = bounded(id);
        self.running.retain(|r| r != id);
        if on {
            if self.running.len() >= LIST_MAX {
                self.running.pop_front();
            }
            self.running.push_back(id.to_owned());
        }
        match self.list.iter().position(|c| c.id == id) {
            None => false,
            Some(i) => {
                let mut changed = set(&mut self.list[i].running, on);
                if on && i > 0 {
                    self.list[..=i].rotate_right(1);
                    changed = true;
                }
                changed
            }
        }
    }

    /// `GET /conversations` (gateway order), the first [`LIST_MAX`] kept.
    pub fn set_list(&mut self, items: &[Conversation]) -> bool {
        let list: Vec<ConvItem> = items
            .iter()
            .take(LIST_MAX)
            .map(|c| ConvItem {
                id: bounded(&c.id).to_owned(),
                title: c
                    .title
                    .as_deref()
                    .map(|t| cut(t, TITLE_MAX_BYTES).to_owned()),
                private: c.owner_user_id.is_some(),
                last_turn_at: c.last_turn_at.as_deref().map(|t| bounded(t).to_owned()),
                running: self.running.iter().any(|r| *r == bounded(&c.id)),
            })
            .collect();
        set(&mut self.list, list)
    }

    /// `GET /conversations/{id}` arrived for the conversation being opened. `false` = not
    /// awaited (stale).
    pub fn on_state(&mut self, id: &str, st: ConversationState) -> bool {
        let Some(open) = self.open.as_mut().filter(|o| o.id == bounded(id)) else {
            return false;
        };
        let Phase::State { buf } = &mut open.phase else {
            return false;
        };
        let mut buf = std::mem::take(buf);
        let as_of = st.as_of;
        buf.evs.retain(|(id, _)| id.is_none_or(|i| i > as_of));
        let mut pending: Vec<MsgEntry> = st
            .pending
            .iter()
            .filter(|m| m.state == MsgState::Pending)
            .map(msg_entry)
            .collect();
        pending.drain(..pending.len().saturating_sub(MAX_ENTRIES));
        open.phase = Phase::History {
            as_of,
            turn: st.turn.as_ref().map(turn_entry),
            pending,
            buf,
        };
        let running = st.turn.is_some();
        self.set_running(id, running);
        true
    }

    /// `GET …/messages` arrived: the conversation is shown and goes live. `false` = stale.
    pub fn on_history(&mut self, id: &str, h: History) -> bool {
        let Some(open) = self
            .open
            .as_mut()
            .filter(|o| o.id == bounded(id) && matches!(o.phase, Phase::History { .. }))
        else {
            return false;
        };
        let Phase::History {
            as_of,
            turn,
            pending,
            buf,
        } = std::mem::replace(&mut open.phase, Phase::Live { as_of: 0 })
        else {
            return false;
        };
        // Turns that ended before this reply: history has them (their reply included).
        let ended: Vec<&str> = buf
            .evs
            .iter()
            .filter_map(|(_, e)| match e {
                Event::TurnEnd(t) => Some(t.turn_id.as_str()),
                _ => None,
            })
            .collect();
        // Turns that started before it: history has their user message. GUESSED: one run at a
        // time, so it is the last delivered message before that turn_start.
        let mut delivered: Vec<&str> = Vec::new();
        let mut claimed: Vec<&str> = Vec::new();
        for (_, e) in &buf.evs {
            match e {
                Event::Message(m) if m.state == MsgState::Delivered => delivered.push(&m.id),
                Event::TurnStart(t) if t.origin != Some(TurnOrigin::Push) => {
                    claimed.extend(delivered.pop());
                }
                _ => {}
            }
        }
        let mut entries: Vec<Entry> = h
            .messages
            .into_iter()
            .filter_map(|m| match m.role {
                Role::User => Some(Entry::Msg(MsgEntry {
                    id: String::new(),
                    text: m.text,
                    mine: false,
                    from_name: None,
                    status: MsgStatus::Past,
                    error_code: None,
                })),
                Role::Assistant => Some(Entry::Turn(TurnEntry {
                    turn_id: String::new(),
                    push: false,
                    by_name: None,
                    by_self: false,
                    reply: m.text,
                    progress: None,
                    tool: None,
                    outcome: Some(TurnOutcome::Done),
                    error_code: None,
                })),
                Role::Other => None,
            })
            .collect();
        entries.extend(
            turn.filter(|t| !ended.contains(&t.turn_id.as_str()))
                .map(Entry::Turn),
        );
        entries.extend(
            pending
                .into_iter()
                .filter(|m| !claimed.contains(&m.id.as_str()))
                .map(Entry::Msg),
        );
        trim(&mut entries);
        for (_, e) in &buf.evs {
            let skip = match e {
                Event::Message(m) => claimed.contains(&m.id.as_str()),
                e => turn_id_of(e).is_some_and(|t| ended.contains(&t)),
            };
            if !skip {
                apply(&mut entries, e);
            }
        }
        open.entries = entries;
        open.phase = Phase::Live { as_of };
        true
    }

    /// A `POST /conversations` is in flight: hold the events of unknown conversations.
    pub fn begin_create(&mut self) {
        self.creating = Some(Buffer::default());
    }

    /// The create failed: nothing to hold any more.
    pub fn create_failed(&mut self) {
        self.creating = None;
    }

    /// The `201` of our create: the new conversation goes to the top and opens. With its early
    /// events intact it opens live at once (nothing to fetch); else `reopen` asks for a normal
    /// open.
    pub fn created(&mut self, c: &Conversation) -> Effect {
        let buf = self.creating.take().unwrap_or_default();
        let id = bounded(&c.id).to_owned();
        let mut list = vec![ConvItem {
            id: id.clone(),
            title: c
                .title
                .as_deref()
                .map(|t| cut(t, TITLE_MAX_BYTES).to_owned()),
            private: c.owner_user_id.is_some(),
            last_turn_at: c.last_turn_at.as_deref().map(|t| bounded(t).to_owned()),
            running: self.running.contains(&id),
        }];
        list.extend(self.list.drain(..).filter(|x| x.id != id));
        list.truncate(LIST_MAX);
        self.list = list;
        self.open(&id);
        if buf.overflowed {
            return Effect {
                view: true,
                list: true,
                reopen: true,
                ..Effect::default()
            };
        }
        let mut entries = Vec::new();
        for (_, e) in buf.evs.iter().filter(|(_, e)| conv_of(e) == Some(&c.id)) {
            apply(&mut entries, e);
        }
        if let Some(o) = self.open.as_mut() {
            o.entries = entries;
            o.phase = Phase::Live { as_of: 0 };
        }
        Effect {
            view: true,
            list: true,
            ..Effect::default()
        }
    }

    /// Our `POST` of `text` failed with `code`: shown (never sent) instead of vanishing.
    pub fn send_failed(&mut self, text: &str, code: &str) -> bool {
        let Some(o) = self.open.as_mut() else {
            return false;
        };
        o.entries.push(Entry::Msg(MsgEntry {
            id: String::new(),
            text: text.to_owned(),
            mine: true,
            from_name: None,
            status: MsgStatus::Failed,
            error_code: Some(bounded(code).to_owned()),
        }));
        trim(&mut o.entries);
        true
    }

    /// One `/events` event (`id`: its SSE id).
    pub fn on_event(&mut self, id: Option<u64>, ev: Event) -> Effect {
        let mut fx = Effect::default();
        let Some(conv) = conv_of(&ev).map(|c| bounded(c).to_owned()) else {
            return fx;
        };
        match &ev {
            Event::TurnStart(_) => fx.list = self.set_running(&conv, true),
            Event::TurnEnd(_) => fx.list = self.set_running(&conv, false),
            _ => {}
        }
        let is_open = self.open_id() == Some(conv.as_str());
        let known = self.list.iter().any(|c| c.id == conv);
        fx.refresh_list = !known && !is_open;
        if let Some(o) = self.open.as_mut().filter(|_| is_open) {
            match &mut o.phase {
                Phase::State { buf } => fx.reopen = buf.push(id, ev),
                Phase::History { as_of, buf, .. } => {
                    if id.is_none_or(|i| i > *as_of) {
                        fx.reopen = buf.push(id, ev);
                    }
                }
                Phase::Live { as_of } => {
                    if id.is_none_or(|i| i > *as_of) {
                        fx.view = apply(&mut o.entries, &ev);
                    }
                }
            }
        } else if let Some(buf) = self.creating.as_mut().filter(|_| !known) {
            buf.push(id, ev);
        }
        fx
    }

    /// The open conversation as the UI draws it today (IPC v5): a person's message and the turn
    /// it started share one `TurnView` (`mine` + `reply`). GUESSED pairing (run ids are not turn
    /// ids): a person's turn takes the last delivered message before it.
    pub fn render(&self, v: &mut ConversationView) -> bool {
        let mut out: Vec<TurnView> = Vec::new();
        if let Some(o) = &self.open {
            let conv = Some(o.id.clone());
            // Indices in `out` of messages that may still get their turn.
            let mut waiting: Vec<usize> = Vec::new();
            for e in &o.entries {
                match e {
                    Entry::Msg(m) => {
                        let mut tv = TurnView::default();
                        tv.conversation = conv.clone();
                        tv.source = TurnSource::Mine;
                        tv.mine = m.text.clone();
                        match m.status {
                            MsgStatus::Pending => {}
                            MsgStatus::Delivered => waiting.push(out.len()),
                            MsgStatus::Past => {
                                tv.outcome = Some(TurnOutcome::Done);
                                waiting.push(out.len());
                            }
                            MsgStatus::Failed => {
                                tv.outcome = Some(TurnOutcome::Error);
                                tv.error_code = m.error_code.clone();
                            }
                        }
                        out.push(tv);
                    }
                    Entry::Turn(t) => {
                        let slot = if t.push { None } else { waiting.pop() };
                        // Earlier messages that got no turn of their own joined one: done.
                        for i in waiting.drain(..) {
                            out[i].outcome.get_or_insert(TurnOutcome::Done);
                        }
                        let tv = match slot {
                            Some(i) => &mut out[i],
                            None => {
                                let mut tv = TurnView::default();
                                tv.conversation = conv.clone();
                                tv.source = if t.push {
                                    TurnSource::Push
                                } else {
                                    TurnSource::Mine
                                };
                                out.push(tv);
                                let last = out.len() - 1;
                                &mut out[last]
                            }
                        };
                        tv.turn_id = t.turn_id.clone();
                        tv.reply = t.reply.clone();
                        tv.progress = t.progress;
                        tv.tool = t.tool.clone();
                        tv.outcome = t.outcome;
                        tv.error_code = t.error_code.clone();
                    }
                }
            }
        }
        trim_view(&mut out);
        set(&mut v.turns, out)
    }
}

// ---- the view's other fields ------------------------------------------------------------

fn view_text_bytes(turns: &[TurnView]) -> usize {
    turns.iter().map(|t| t.mine.len() + t.reply.len()).sum()
}

/// Drops the oldest turns until both bounds of the view hold; a single turn over the text bound
/// has its reply cut (at a char boundary).
fn trim_view(turns: &mut Vec<TurnView>) {
    let excess = turns.len().saturating_sub(CONVERSATION_MAX_TURNS);
    turns.drain(..excess);
    while turns.len() > 1 && view_text_bytes(turns) > CONVERSATION_MAX_TEXT_BYTES {
        turns.remove(0);
    }
    if let Some(t) = turns.first_mut() {
        truncate(&mut t.mine, CONVERSATION_MAX_TEXT_BYTES);
        let room = CONVERSATION_MAX_TEXT_BYTES.saturating_sub(t.mine.len());
        truncate(&mut t.reply, room);
    }
}

/// The session ended: every open turn is `Closed`, the voice state is unknown again.
// shortcut: the engine of PR 4a Task 4 stops calling it (a resumed `/events` replays what was
// missed); removed with the old engine wiring.
pub fn link_down(v: &mut ConversationView) -> bool {
    let mut changed = false;
    for t in v.turns.iter_mut().filter(|t| t.outcome.is_none()) {
        t.outcome = Some(TurnOutcome::Closed);
        t.progress = None;
        t.tool = None;
        changed = true;
    }
    changed |= set(&mut v.transcriber, ServiceState::Unknown);
    changed | set(&mut v.tts, ServiceState::Unknown)
}

pub fn set_mic(v: &mut ConversationView, m: MicPermission) -> bool {
    set(&mut v.mic, m)
}

pub fn set_activity(v: &mut ConversationView, a: fogar_ports::ipc::UserActivity) -> bool {
    set(&mut v.activity, a)
}

/// The take ended: no live line; `notice` says why when it was cut automatically.
pub fn take_ended(v: &mut ConversationView, notice: Option<Notice>) -> bool {
    let mut changed = !v.live.is_empty();
    v.live.clear();
    if notice.is_some() {
        changed |= set(&mut v.notice, notice);
    }
    changed
}

/// The wait for our turn ended without a `turn_start`.
// shortcut: only the old turn machine calls it; removed with it (PR 4a Task 4).
pub fn wait_ended(v: &mut ConversationView, o: TurnOutcome) -> bool {
    match v
        .turns
        .iter_mut()
        .rev()
        .find(|t| t.turn_id.is_empty() && t.outcome.is_none() && !t.mine.is_empty())
    {
        Some(t) => {
            t.outcome = Some(o);
            true
        }
        None => false,
    }
}

/// Ends every open turn that has a gateway id.
// shortcut: only the old turn machine calls it; removed with it (PR 4a Task 4).
pub fn end_open(v: &mut ConversationView, o: TurnOutcome) -> bool {
    let mut changed = false;
    for t in v
        .turns
        .iter_mut()
        .filter(|t| t.outcome.is_none() && !t.turn_id.is_empty())
    {
        t.outcome = Some(o);
        changed = true;
    }
    changed
}

/// The playback gate's state: whether audio plays now and which turn waits for `TurnVisible`.
pub fn playback(v: &mut ConversationView, speaking: bool, awaiting: Option<&str>) -> bool {
    let mut changed = set(&mut v.speaking, speaking);
    if v.awaiting_visible.as_deref() != awaiting {
        v.awaiting_visible = awaiting.map(str::to_owned);
        changed = true;
    }
    changed
}

/// The audio of turn `view_id` (as the view shows it) played, or was muted. `Played` is never
/// turned into `Muted`.
pub fn turn_audio(v: &mut ConversationView, view_id: &str, audio: TurnAudio) -> bool {
    if view_id.is_empty() {
        return false;
    }
    match v.turns.iter_mut().rev().find(|t| t.turn_id == view_id) {
        Some(t) if t.audio == TurnAudio::Played => false,
        Some(t) => set(&mut t.audio, audio),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use fogar_proto::events::decode_event;
    use serde_json::json;

    fn ev(v: serde_json::Value) -> Event {
        decode_event(&v.to_string())
    }
    fn msg(conv: &str, id: &str, text: &str, state: &str, self_: bool) -> Event {
        ev(
            json!({"type":"message","id":id,"conversation":conv,"text":text,"state":state,
            "from":{"source":"device","device_id":"d","name":"Cocina","self":self_}}),
        )
    }
    fn start(conv: &str, turn: &str) -> Event {
        ev(
            json!({"type":"turn_start","turn_id":turn,"conversation":conv,"origin":"user",
            "by":{"device_id":"d","name":"Cocina","self":false}}),
        )
    }
    fn push_start(conv: &str, turn: &str) -> Event {
        ev(
            json!({"type":"turn_start","turn_id":turn,"conversation":conv,"origin":"push","by":null}),
        )
    }
    fn token(conv: &str, turn: &str, text: &str) -> Event {
        ev(json!({"type":"token","turn_id":turn,"conversation":conv,"text":text}))
    }
    fn end_ev(conv: &str, turn: &str) -> Event {
        ev(json!({"type":"turn_end","turn_id":turn,"conversation":conv,"reason":"done"}))
    }
    fn conv(id: &str) -> Conversation {
        serde_json::from_value(json!({"id":id,"agent":"main","created_at":"x","title":"T"}))
            .unwrap()
    }
    fn state(
        id: &str,
        as_of: u64,
        turn: Option<(&str, &str)>,
        pending: &[(&str, &str)],
    ) -> ConversationState {
        let turn = turn.map(|(t, text)| {
            json!({"turn_id":t,"conversation":id,"origin":"user","by":null,"text":text})
        });
        let pending: Vec<_> = pending
            .iter()
            .map(|(m, text)| {
                json!({"id":m,"conversation":id,"text":text,"state":"pending",
                "from":{"source":"device","self":true}})
            })
            .collect();
        serde_json::from_value(json!({"id":id,"agent":"main","created_at":"x",
            "turn":turn,"pending":pending,"as_of":as_of}))
        .unwrap()
    }
    fn history(id: &str, msgs: &[(&str, &str)]) -> History {
        let messages: Vec<_> = msgs
            .iter()
            .map(|(role, text)| json!({"id":"h","role":role,"text":text}))
            .collect();
        serde_json::from_value(json!({"conversation_id":id,"messages":messages,"has_more":false}))
            .unwrap()
    }
    /// `(mine, reply)` of every view turn.
    fn shown(c: &Chat) -> Vec<(String, String)> {
        let mut v = ConversationView::default();
        c.render(&mut v);
        v.turns.into_iter().map(|t| (t.mine, t.reply)).collect()
    }
    fn pair(m: &str, r: &str) -> (String, String) {
        (m.to_owned(), r.to_owned())
    }
    /// `c-1` opened live with `msgs` of history at `as_of`.
    fn opened(as_of: u64, msgs: &[(&str, &str)]) -> Chat {
        let mut c = Chat::default();
        c.set_list(&[conv("c-1")]);
        c.open("c-1");
        assert!(c.on_state("c-1", state("c-1", as_of, None, &[])));
        assert!(c.on_history("c-1", history("c-1", msgs)));
        c
    }

    #[test]
    fn history_pairs_messages_with_replies() {
        let c = opened(10, &[("user", "hola"), ("assistant", "qué tal")]);
        assert_eq!(shown(&c), [pair("hola", "qué tal")]);
        assert!(c.open.as_ref().unwrap().is_live());
    }

    /// Review focus 1: events up to `as_of` are already in the state; they never apply twice.
    #[test]
    fn events_up_to_as_of_are_dropped() {
        let mut c = opened(10, &[("user", "hola"), ("assistant", "qué tal")]);
        for id in [9, 10] {
            let fx = c.on_event(Some(id), msg("c-1", "r-1", "viejo", "delivered", false));
            assert!(!fx.view, "{id}");
        }
        let fx = c.on_event(Some(11), msg("c-1", "r-2", "nuevo", "delivered", false));
        assert!(fx.view);
        assert_eq!(shown(&c)[1], pair("nuevo", ""));
        // The same filter while opening: buffered events up to `as_of` go.
        let mut c = Chat::default();
        c.open("c-1");
        c.on_event(Some(5), msg("c-1", "r-1", "viejo", "delivered", false));
        c.on_event(Some(12), msg("c-1", "r-2", "nuevo", "pending", false));
        c.on_state("c-1", state("c-1", 10, None, &[]));
        c.on_event(Some(9), msg("c-1", "r-3", "viejo", "delivered", false));
        c.on_history("c-1", history("c-1", &[]));
        assert_eq!(shown(&c), [pair("nuevo", "")]);
    }

    /// Review focus 1: everything that arrives while the state and the history are in flight is
    /// shown once, in order, after them.
    #[test]
    fn open_during_live_events_loses_nothing() {
        let mut c = Chat::default();
        c.set_list(&[conv("c-1")]);
        c.open("c-1");
        // A turn runs; a second message waits behind it.
        c.on_event(Some(21), token("c-1", "t-1", " mundo"));
        c.on_state(
            "c-1",
            state("c-1", 20, Some(("t-1", "Hola")), &[("r-2", "otra")]),
        );
        c.on_event(Some(22), token("c-1", "t-1", "!"));
        // The history (fetched after the state) has t-1's message, not its reply.
        c.on_history("c-1", history("c-1", &[("user", "uno")]));
        assert_eq!(shown(&c), [pair("uno", "Hola mundo!"), pair("otra", "")]);
        c.on_event(Some(23), end_ev("c-1", "t-1"));
        c.on_event(Some(24), msg("c-1", "r-2", "otra", "delivered", true));
        c.on_event(Some(25), start("c-1", "t-2"));
        c.on_event(Some(26), token("c-1", "t-2", "vale"));
        assert_eq!(
            shown(&c),
            [pair("uno", "Hola mundo!"), pair("otra", "vale")]
        );
        let mut v = ConversationView::default();
        c.render(&mut v);
        assert_eq!(v.turns[0].outcome, Some(TurnOutcome::Done));
        assert_eq!(
            (v.turns[1].turn_id.as_str(), v.turns[1].outcome),
            ("t-2", None)
        );
    }

    /// The rule for a turn ending while the conversation opens (GUESSED order): ended before
    /// the history reply -> history's copy; ended after -> the live copy. Never both.
    #[test]
    fn turn_finishing_during_open_is_shown_once() {
        // (a) The running turn of the state ends before the history reply.
        let mut c = Chat::default();
        c.open("c-1");
        c.on_state("c-1", state("c-1", 20, Some(("t-1", "Hola")), &[]));
        c.on_event(Some(21), token("c-1", "t-1", " mundo"));
        c.on_event(Some(22), end_ev("c-1", "t-1"));
        c.on_history(
            "c-1",
            history("c-1", &[("user", "uno"), ("assistant", "Hola mundo")]),
        );
        assert_eq!(shown(&c), [pair("uno", "Hola mundo")]);
        // (b) A whole turn (message, start, reply, end) inside the window.
        let mut c = Chat::default();
        c.open("c-1");
        c.on_state("c-1", state("c-1", 20, None, &[]));
        c.on_event(Some(21), msg("c-1", "r-1", "dos", "delivered", false));
        c.on_event(Some(22), start("c-1", "t-2"));
        c.on_event(Some(23), token("c-1", "t-2", "ok"));
        c.on_event(Some(24), end_ev("c-1", "t-2"));
        c.on_history(
            "c-1",
            history("c-1", &[("user", "dos"), ("assistant", "ok")]),
        );
        assert_eq!(shown(&c), [pair("dos", "ok")]);
        // (c) It ends after the history reply: the live copy, once.
        let mut c = Chat::default();
        c.open("c-1");
        c.on_state("c-1", state("c-1", 20, Some(("t-1", "Ho")), &[]));
        c.on_history("c-1", history("c-1", &[("user", "uno")]));
        c.on_event(Some(21), token("c-1", "t-1", "la"));
        c.on_event(Some(22), end_ev("c-1", "t-1"));
        assert_eq!(shown(&c), [pair("uno", "Hola")]);
        // (d) Started (not ended) before the history reply: its message comes from history.
        let mut c = Chat::default();
        c.open("c-1");
        c.on_state("c-1", state("c-1", 20, None, &[("r-1", "dos")]));
        c.on_event(Some(21), msg("c-1", "r-1", "dos", "delivered", true));
        c.on_event(Some(22), start("c-1", "t-2"));
        c.on_history("c-1", history("c-1", &[("user", "dos")]));
        c.on_event(Some(23), token("c-1", "t-2", "ok"));
        assert_eq!(shown(&c), [pair("dos", "ok")]);
    }

    #[test]
    fn stale_or_unawaited_replies_change_nothing() {
        let mut c = Chat::default();
        assert!(!c.on_state("c-1", state("c-1", 1, None, &[])));
        c.open("c-2");
        assert!(!c.on_state("c-1", state("c-1", 1, None, &[])));
        assert!(!c.on_history("c-2", history("c-2", &[])), "state first");
        assert!(c.on_state("c-2", state("c-2", 1, None, &[])));
        assert!(!c.on_state("c-2", state("c-2", 1, None, &[])), "once");
    }

    #[test]
    fn open_buffer_overflow_asks_to_reopen_once() {
        let mut c = Chat::default();
        c.set_list(&[conv("c-1")]);
        c.open("c-1");
        let mut reopen = 0;
        for i in 0..(BUFFER_EVENTS as u64 + 10) {
            reopen += usize::from(c.on_event(Some(i), token("c-1", "t", "x")).reopen);
        }
        assert_eq!(reopen, 1);
        let big = "x".repeat(BUFFER_BYTES);
        c.open("c-1");
        assert!(c.on_event(Some(1), token("c-1", "t", &big)).reopen);
    }

    #[test]
    fn other_device_message_shows_its_name() {
        let mut c = opened(1, &[]);
        c.on_event(
            Some(2),
            msg("c-1", "r-1", "pon la serie", "delivered", false),
        );
        let Entry::Msg(m) = &c.open.as_ref().unwrap().entries[0] else {
            panic!()
        };
        assert_eq!((m.mine, m.from_name.as_deref()), (false, Some("Cocina")));
        c.on_event(Some(3), msg("c-1", "r-2", "mío", "delivered", true));
        let Entry::Msg(m) = &c.open.as_ref().unwrap().entries[1] else {
            panic!()
        };
        assert!(m.mine);
    }

    #[test]
    fn pending_then_delivered_then_withdrawn_disappears() {
        let mut c = opened(1, &[]);
        c.on_event(Some(2), start("c-1", "t-1"));
        c.on_event(Some(3), msg("c-1", "r-1", "espera", "pending", true));
        let status = |c: &Chat| match &c.open.as_ref().unwrap().entries[1] {
            Entry::Msg(m) => m.status,
            Entry::Turn(_) => panic!(),
        };
        assert_eq!(status(&c), MsgStatus::Pending);
        c.on_event(Some(4), msg("c-1", "r-1", "espera", "delivered", true));
        assert_eq!(status(&c), MsgStatus::Delivered);
        c.on_event(Some(5), msg("c-1", "r-2", "otra", "pending", false));
        // Withdrawn by whoever: gone from the view.
        assert!(
            c.on_event(Some(6), msg("c-1", "r-2", "otra", "withdrawn", false))
                .view
        );
        assert_eq!(c.open.as_ref().unwrap().entries.len(), 2);
        assert!(
            !c.on_event(Some(7), msg("c-1", "r-9", "x", "withdrawn", false))
                .view
        );
    }

    /// Sito: a conversation where any device talks shows running and moves to the top; the
    /// marks survive a refresh of the list; events of other conversations keep no text.
    #[test]
    fn list_marks_running_and_moves_to_top() {
        let mut c = Chat::default();
        assert!(c.set_list(&[conv("c-1"), conv("c-2"), conv("c-3")]));
        let fx = c.on_event(Some(1), start("c-3", "t-1"));
        assert!(fx.list && !fx.view && !fx.refresh_list);
        let ids: Vec<_> = c.list.iter().map(|i| (i.id.as_str(), i.running)).collect();
        assert_eq!(ids, [("c-3", true), ("c-1", false), ("c-2", false)]);
        c.set_list(&[conv("c-1"), conv("c-3"), conv("c-2")]);
        assert!(c.list[1].running, "kept across a refresh");
        assert!(c.on_event(Some(2), end_ev("c-3", "t-1")).list);
        assert!(c.list.iter().all(|i| !i.running));
        // Unknown conversation: refresh; nothing is kept.
        let fx = c.on_event(Some(3), msg("c-9", "r", "secreto", "delivered", false));
        assert!(fx.refresh_list && !fx.view);
        assert!(!format!("{c:?}").contains("secreto"));
        // At most LIST_MAX items.
        let many: Vec<_> = (0..80).map(|i| conv(&format!("c-{i}"))).collect();
        c.set_list(&many);
        assert_eq!(c.list.len(), LIST_MAX);
    }

    /// Creating with a message: its `message` arrives before the 201; the conversation opens
    /// live with it (matched by id, no local echo).
    #[test]
    fn created_conversation_opens_with_its_early_events() {
        let mut c = Chat::default();
        c.set_list(&[conv("c-1")]);
        c.begin_create();
        c.on_event(Some(5), msg("c-new", "r-1", "hola", "delivered", true));
        c.on_event(Some(6), token("c-1", "t-x", "otro"));
        let fx = c.created(&conv("c-new"));
        assert!(fx.view && fx.list && !fx.reopen);
        assert_eq!(c.list[0].id, "c-new");
        assert_eq!(c.open_id(), Some("c-new"));
        c.on_event(Some(7), start("c-new", "t-1"));
        c.on_event(Some(8), token("c-new", "t-1", "buenas"));
        assert_eq!(shown(&c), [pair("hola", "buenas")]);
        // Overflowed while creating: a normal open.
        let mut c = Chat::default();
        c.begin_create();
        for i in 0..(BUFFER_EVENTS as u64 + 1) {
            c.on_event(Some(i), token("c-new", "t", "x"));
        }
        let fx = c.created(&conv("c-new"));
        assert!(fx.reopen);
        assert!(!c.open.as_ref().unwrap().is_live());
    }

    #[test]
    fn push_turns_and_errors() {
        let mut c = opened(1, &[]);
        c.on_event(Some(2), push_start("c-1", "t-1"));
        c.on_event(Some(3), token("c-1", "t-1", "recordatorio"));
        c.on_event(Some(4), end_ev("c-1", "t-1"));
        c.on_event(Some(5), msg("c-1", "r-1", "hola", "delivered", true));
        c.on_event(Some(6), start("c-1", "t-2"));
        c.on_event(
            Some(7),
            ev(
                json!({"type":"error","code":"TURN_ERROR","message":"orchestrator_unavailable",
                "turn_id":"t-2","conversation":"c-1"}),
            ),
        );
        let mut v = ConversationView::default();
        c.render(&mut v);
        assert_eq!(v.turns[0].source, TurnSource::Push);
        assert_eq!(v.turns[0].reply, "recordatorio");
        assert_eq!(
            (
                v.turns[1].mine.as_str(),
                v.turns[1].outcome,
                v.turns[1].error_code.as_deref()
            ),
            (
                "hola",
                Some(TurnOutcome::Error),
                Some("orchestrator_unavailable")
            )
        );
        assert!(c.send_failed("no salió", "missing_scope"));
        c.render(&mut v);
        assert_eq!(v.turns[2].error_code.as_deref(), Some("missing_scope"));
    }

    /// Review focus 5: the view stays within 48 KiB (and 20 turns) whatever arrives.
    #[test]
    fn view_stays_within_48_kib() {
        let mut c = opened(1, &[]);
        let chunk = "é".repeat(3000);
        for i in 0..200u64 {
            let t = format!("t-{i}");
            c.on_event(
                Some(10 * i + 2),
                msg("c-1", &format!("r-{i}"), &chunk, "delivered", true),
            );
            c.on_event(Some(10 * i + 3), start("c-1", &t));
            for k in 0..4 {
                c.on_event(Some(10 * i + 4 + k), token("c-1", &t, &chunk));
            }
            let o = c.open.as_ref().unwrap();
            assert!(o.entries.len() <= MAX_ENTRIES);
            assert!(text_bytes(&o.entries) <= CONVERSATION_MAX_TEXT_BYTES);
        }
        let mut v = ConversationView::default();
        c.render(&mut v);
        assert!(v.turns.len() <= CONVERSATION_MAX_TURNS);
        assert!(view_text_bytes(&v.turns) <= CONVERSATION_MAX_TEXT_BYTES);
        // One reply alone over the bound is cut at a char boundary.
        let mut c = opened(1, &[]);
        c.on_event(Some(2), push_start("c-1", "t"));
        for i in 0..40 {
            c.on_event(Some(3 + i), token("c-1", "t", &chunk));
        }
        c.render(&mut v);
        assert_eq!(v.turns.len(), 1);
        assert!(view_text_bytes(&v.turns) <= CONVERSATION_MAX_TEXT_BYTES);
        assert!(v.turns[0].reply.chars().all(|ch| ch == 'é'));
    }

    #[test]
    fn debug_prints_sizes_never_content() {
        let mut c = opened(1, &[("user", "mi PIN es 4321")]);
        c.on_event(Some(2), push_start("c-1", "t-1"));
        c.on_event(Some(3), token("c-1", "t-1", "secreto 9876"));
        c.on_event(Some(4), msg("c-1", "r", "clave 5555", "pending", false));
        c.set_list(&[serde_json::from_value(
            json!({"id":"c-1","agent":"main","created_at":"x","title":"título 7777"}),
        )
        .unwrap()]);
        let d = format!("{c:?}");
        for secret in ["4321", "9876", "5555", "7777", "Cocina"] {
            assert!(!d.contains(secret), "{secret}: {d}");
        }
    }

    #[test]
    fn clear_forgets_everything() {
        let mut c = opened(1, &[("user", "x")]);
        assert!(c.clear());
        assert_eq!(c, Chat::default());
        assert!(!c.clear());
    }
}
