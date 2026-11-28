//! Gateway link supervisor over `GET /events` (gateway 5.0, client-protocol §16): ONE SSE stream
//! per device, open whenever there is an account (fogar-longrun-reliability "connection
//! supervisor", with an HTTP stream instead of a WebSocket).
//!
//! - Attempt: `open_stream` under `connect_timeout`; a non-2xx status is read with
//!   [`read_capped`] and classified by [`classify_http`] (401 signs out through
//!   `revoke_if_current`, `403 missing_scope` parks, the rest backs off or parks). Then only
//!   `ready` counts, within `ready_timeout`; a `ready.protocol` other than ours parks
//!   (`UpgradeClient`).
//! - Read loop: every `chunk()` runs under `events_read_timeout` (any byte, `: ping` included,
//!   resets it). Events go to the engine must-deliver (byte charge + slot, each under
//!   `inbound_deadline`); `last_id` advances only once the engine accepted the event, survives
//!   reconnects and is dropped when the account changes.
//! - Rotation: the gateway ends every stream after ~25 s. A clean end after `ready` on a stream
//!   that lasted `stable_stream` reopens at once with `Last-Event-ID`; if that reaches `ready`
//!   within `rotation_grace`, nothing above the link sees it (no `Down`, no `Up`, no state or
//!   status change). Anything else is a real `Down`, then the jittered backoff.
//! - `resync` events and oversize events (skipped by the reader) ask the engine to re-fetch
//!   (`Inbound::Resync`). Push `audio` is dropped here (counted): nothing plays in 4a.
//!
//! Ownership: ONE supervisor task, no per-stream task: the body is read inline and dropped (the
//! connection closes) when the stream ends. Cancellation: `egress` drops the stream and stops.

use crate::{
    account::{self, AccountRecord},
    config::Config,
    queues::{ByteBudget, Charge},
    stats::{LinkState, Stats},
    time::deadline,
};
use fogar_ports::{
    clock::{Clock, Mono},
    gateway::GatewayAddr,
    gateway_http::{GatewayHttp, HttpCall, MAX_BODY_BYTES, ResponseBody},
    ipc::GatewayInfo,
    random::Random,
    secrets::{SecretError, SecretStore, SecretString},
};
use fogar_proto::{
    CloseAction,
    account::ApiError,
    client::PROTOCOL_VERSION,
    events::{Event, decode_event},
    sse::{SseItem, SseReader},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

/// What the engine needs to make REST calls for the signed-in account. `Debug` shows the
/// address only.
#[derive(Clone)]
pub(crate) struct Api {
    pub(crate) addr: GatewayAddr,
    pub(crate) token: SecretString,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api").field("addr", &self.addr).finish()
    }
}

/// Link -> engine. `Event` carries the [`Charge`] of its bytes against `queues::INBOUND_BYTES`;
/// it is released when the engine drops the message. `Debug` hides every text (`Event`'s own).
#[derive(Debug)]
pub(crate) enum Inbound {
    /// A stream reached `ready` after a `Down` (or for the first time). `resumed`: it was asked
    /// to resume (`Last-Event-ID`); a `Resync` follows if the gateway could not.
    Up { api: Api, resumed: bool },
    /// The stream is gone and the planned rotation could not replace it in time.
    Down,
    /// `id`: the SSE id (`None` for events without one); `bytes`: its SSE data length.
    Event {
        ev: Event,
        id: Option<u64>,
        bytes: usize,
        charge: Charge,
    },
    /// Re-fetch what is shown: the gateway lost our place, or an event was too big to read.
    Resync,
    /// There is no account (signed out, revoked): forget everything shown.
    SignedOut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkView {
    pub state: LinkState,
    pub last_close: Option<(u16, CloseAction)>,
    /// The gateway of the account the link is using; `None` = signed out.
    pub gateway: Option<GatewayInfo>,
}

/// What a REST status means for the caller (the link and, in Task 4, the engine's calls).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HttpClass {
    Ok,
    /// `401 not_authenticated`: the token is dead: sign out.
    Revoked,
    /// `403 missing_scope`: the token is valid but lacks the scope; never retried.
    MissingScope,
    /// 5xx, 408, 425, 429, or a body that cannot be read or decoded: back off and retry.
    Retry,
    /// Any other answer: the same request fails the same way, do not retry.
    Fatal,
}

/// One shared reading of a REST status (+ its capped body, `None` if it could not be read).
pub(crate) fn classify_http(status: u16, body: Option<&[u8]>) -> HttpClass {
    match status {
        200..=299 => HttpClass::Ok,
        // Only the gateway's own answer signs out; any other 401 (a proxy page) backs off.
        401 => match body.and_then(|b| serde_json::from_slice::<ApiError>(b).ok()) {
            Some(e) if e.code() == Some("not_authenticated") => HttpClass::Revoked,
            _ => HttpClass::Retry,
        },
        403 => match body.and_then(|b| serde_json::from_slice::<ApiError>(b).ok()) {
            Some(e) if e.code() == Some("missing_scope") => HttpClass::MissingScope,
            Some(_) => HttpClass::Fatal,
            // Undecodable (a proxy page?): back off, never a tight loop.
            None => HttpClass::Retry,
        },
        408 | 425 | 429 | 500..=599 => HttpClass::Retry,
        300..=499 => HttpClass::Fatal,
        _ => HttpClass::Retry,
    }
}

/// Reads a whole (non-2xx) body of at most `max` bytes, every `chunk()` inside the one deadline
/// `d`. `None`: larger, broken or too slow.
pub(crate) async fn read_capped<C: Clock, B: ResponseBody>(
    clock: &C,
    body: &mut B,
    max: usize,
    d: Duration,
) -> Option<Vec<u8>> {
    let read = async {
        let mut out = Vec::new();
        loop {
            match body.chunk().await {
                Ok(Some(c)) if out.len() + c.len() <= max => out.extend_from_slice(&c),
                Ok(Some(_)) | Err(_) => return None,
                Ok(None) => return Some(out),
            }
        }
    };
    deadline(clock, d, read).await.flatten()
}

pub(crate) struct LinkCtx<C, H> {
    pub(crate) clock: Arc<C>,
    pub(crate) http: Arc<H>,
    pub(crate) secrets: Arc<dyn SecretStore>,
    pub(crate) random: Arc<dyn Random>,
    pub(crate) cfg: Arc<Config>,
    pub(crate) stats: Arc<Stats>,
    pub(crate) view: watch::Sender<LinkView>,
    /// Byte budget of the inbound queue (`queues::INBOUND_BYTES`).
    pub(crate) budget: ByteBudget,
    /// Bumped whenever the account is written or deleted: the link drops what it is
    /// doing (stream, backoff, park) and starts over from the stored account.
    pub(crate) account: watch::Receiver<()>,
    /// Keeps the account channel open while the link runs: a closed `watch` would make every
    /// `changed()` branch above resolve at once (a hot loop).
    pub(crate) _account_keep: watch::Sender<()>,
    /// Bumped by an IPC `Reconnect`; wakes `park` only for [`LinkState::user_may_reconnect`].
    pub(crate) reconnect: watch::Receiver<()>,
    /// Same reason as `_account_keep` (and there is no IPC sender at all without an IPC port).
    pub(crate) _reconnect_keep: watch::Sender<()>,
}

/// Why an attempt ended.
#[derive(Debug)]
enum Outcome {
    Shutdown,
    /// Engine dropped its end: nothing left to serve.
    EngineGone,
    /// The account changed (signed in or out): start over without backoff.
    AccountChanged,
    /// No `ready`: unreachable, refused, a retryable status, EOF / silence before `ready`.
    ConnectFailed,
    /// The stream ended after `ready` (EOF, error, silence) or the engine stalled past its
    /// deadline: back off and resume.
    Ended,
    /// 401 `not_authenticated`.
    Revoked,
    /// Retrying cannot help: wait for the person (or a new account).
    Park(CloseAction),
}

/// One thing read from the stream, owned (the reader's items borrow its buffer).
enum Item {
    Ev {
        ev: Event,
        id: Option<u64>,
        bytes: usize,
    },
    /// A dropped event, with its id when it came before the drop.
    Oversize(Option<u64>),
}

/// Feeds `bytes` to the reader and appends what it found. One chunk is at most
/// `MAX_CHUNK_BYTES`, so `items` stays small; the caller reuses it.
fn collect(reader: &mut SseReader, bytes: &[u8], items: &mut Vec<Item>) {
    reader.feed(bytes, &mut |it| match it {
        SseItem::Event { id, data } => items.push(Item::Ev {
            ev: decode_event(data),
            id: id.and_then(|s| s.parse().ok()),
            bytes: data.len(),
        }),
        SseItem::Oversize { id } => items.push(Item::Oversize(id.and_then(|s| s.parse().ok()))),
        SseItem::Comment => {}
    });
}

/// An open stream past its `ready`.
struct Stream<B> {
    body: B,
    reader: SseReader,
    opened: Mono,
    /// The `ready` event's id (a fresh stream names the current id there).
    ready_id: Option<u64>,
    /// Items that came after `ready` in the same chunk.
    rest: Vec<Item>,
}

/// How a read loop ended.
enum ReadEnd {
    /// The gateway ended the stream cleanly.
    Eof,
    Other(Outcome),
}

impl<C: Clock, H: GatewayHttp> LinkCtx<C, H> {
    fn set_state(&self, state: LinkState) {
        self.view.send_modify(|v| v.state = state);
    }

    /// `false` = shutdown. Returns early (`true`) when the account changes.
    async fn wait(&self, d: Duration, egress: &CancellationToken) -> bool {
        let mut account = self.account.clone();
        tokio::select! {
            biased;
            () = egress.cancelled() => false,
            _ = account.changed() => true,
            () = self.clock.sleep(d) => true,
        }
    }

    fn backoff(&self, attempt: &mut u32) -> Duration {
        let d = self.cfg.backoff.delay(*attempt, &*self.random);
        *attempt = attempt.saturating_add(1);
        d
    }

    pub(crate) async fn run(mut self, in_tx: mpsc::Sender<Inbound>, egress: CancellationToken) {
        let mut attempt: u32 = 0;
        // The account the resume point belongs to, and the resume point.
        let mut owner: Option<AccountRecord> = None;
        let mut last_id: Option<u64> = None;
        // A `SignedOut` the stalled engine missed: delivered before the next `Up`, so the
        // engine never keeps the old account's handle or conversation into a new one.
        let mut signed_out_owed = false;
        loop {
            if egress.is_cancelled() || in_tx.is_closed() {
                break;
            }
            // Changes from here on interrupt whatever the link does with this record.
            self.account.borrow_and_update();
            let record = match account::load(&*self.secrets) {
                Ok(Some(r)) => r,
                // A store this build cannot use (e.g. a stub) will not start working by
                // retrying: show "signed out" instead of "reconnecting" forever.
                Ok(None) | Err(SecretError::Unsupported(_)) => {
                    owner = None;
                    last_id = None;
                    self.view.send_modify(|v| {
                        v.state = LinkState::NotPaired;
                        v.gateway = None;
                    });
                    signed_out_owed = !self.forward(&in_tx, Inbound::SignedOut, &egress).await;
                    if signed_out_owed
                        && !matches!(self.not_delivered(&in_tx, &egress), Outcome::Ended)
                    {
                        break;
                    }
                    // No timer once delivered: the only writers are `Login` (signals the watch)
                    // and the CLI dev path at boot. A signed-out Mac costs no wakeups and no
                    // Keychain reads. While owed, retried on the backoff so a recovered engine
                    // drops the old account without waiting for the next login.
                    let mut account = self.account.clone();
                    while signed_out_owed {
                        let d = self.backoff(&mut attempt);
                        tokio::select! {
                            biased;
                            () = egress.cancelled() => break,
                            r = account.changed() => if r.is_ok() { break } else { return },
                            () = self.clock.sleep(d) => {
                                signed_out_owed =
                                    !self.forward(&in_tx, Inbound::SignedOut, &egress).await;
                                if signed_out_owed
                                    && !matches!(self.not_delivered(&in_tx, &egress), Outcome::Ended)
                                {
                                    return;
                                }
                            }
                        }
                    }
                    if signed_out_owed {
                        // Shutdown or the account changed: the top of the loop decides.
                        continue;
                    }
                    attempt = 0;
                    tokio::select! {
                        biased;
                        () = egress.cancelled() => break,
                        r = account.changed() => if r.is_ok() { continue } else { break },
                    }
                }
                Err(_) => {
                    let d = self.backoff(&mut attempt);
                    self.set_state(LinkState::Backoff {
                        attempt,
                        last_close: None,
                    });
                    if self.wait(d, &egress).await {
                        continue;
                    }
                    break;
                }
            };
            if signed_out_owed {
                if !self.forward(&in_tx, Inbound::SignedOut, &egress).await {
                    if !matches!(self.not_delivered(&in_tx, &egress), Outcome::Ended) {
                        break;
                    }
                    let d = self.backoff(&mut attempt);
                    if self.wait(d, &egress).await {
                        continue;
                    }
                    break;
                }
                signed_out_owed = false;
            }
            if !owner.as_ref().is_some_and(|o| o.same_as(&record)) {
                last_id = None;
            }
            let record = owner.insert(record);
            let info = record.info();
            self.view.send_modify(|v| {
                v.state = LinkState::Connecting { attempt };
                if v.gateway.as_ref() != Some(&info) {
                    v.gateway = Some(info);
                }
            });
            Stats::inc(&self.stats.connect_attempts);
            let (outcome, stable) = self.attempt(record, &mut last_id, &in_tx, &egress).await;
            if stable {
                attempt = 0;
            }
            let delay = match outcome {
                Outcome::Shutdown | Outcome::EngineGone => break,
                Outcome::AccountChanged => {
                    attempt = 0;
                    continue;
                }
                Outcome::Park(action) => {
                    if self.park(action, &egress).await {
                        attempt = 0;
                        continue;
                    }
                    break;
                }
                Outcome::Revoked => {
                    if !account::revoke_if_current(&*self.secrets, record) {
                        // The store cannot be read or the delete failed: never retry a dead
                        // token.
                        if self.park(CloseAction::Repair, &egress).await {
                            attempt = 0;
                            continue;
                        }
                        break;
                    }
                    // Signed out; never retry this token. Still through the backoff: a store
                    // whose delete silently fails must not turn a gateway that refuses every
                    // stream into back-to-back attempts (NET-4).
                    self.view.send_modify(|v| {
                        v.state = LinkState::NotPaired;
                        v.gateway = None;
                        // The status log names the revocation (as it did for WS 4010/4200).
                        v.last_close = Some((401, CloseAction::Repair));
                    });
                    let d = self.backoff(&mut attempt);
                    if self.wait(d, &egress).await {
                        continue;
                    }
                    break;
                }
                Outcome::ConnectFailed => {
                    Stats::inc(&self.stats.connect_failures);
                    self.backoff(&mut attempt)
                }
                // A stable stream reset `attempt` above: a prompt, jittered reconnect. A gateway
                // that ends every stream at once escalates like any other failure (NET-4).
                Outcome::Ended => self.backoff(&mut attempt),
            };
            self.set_state(LinkState::Backoff {
                attempt,
                last_close: None,
            });
            if !self.wait(delay, &egress).await {
                break;
            }
        }
    }

    /// Stays alive but stops retrying (retrying would loop: fogar-longrun "config/auth errors do
    /// not exit"). `true` = the account changed (signed in or out) or the person asked
    /// to reconnect (only for [`LinkState::user_may_reconnect`] reasons): start over;
    /// `false` = shutdown.
    async fn park(&self, reason: CloseAction, egress: &CancellationToken) -> bool {
        let mut account = self.account.clone();
        let mut reconnect = self.reconnect.clone();
        // Load-bearing: `self.reconnect` is never advanced, so a fresh clone still sees every past
        // request as new; without this, each park after the first accepted Reconnect would wake
        // at once (a reconnect loop, NET-4). Before publishing Parked, so a request sent after
        // the IPC service saw Parked does wake it.
        reconnect.borrow_and_update();
        let state = LinkState::Parked { reason };
        let user_may_reconnect = state.user_may_reconnect();
        self.set_state(state);
        tokio::select! {
            biased;
            () = egress.cancelled() => false,
            r = account.changed() => r.is_ok(),
            r = reconnect.changed(), if user_may_reconnect => r.is_ok(),
        }
    }

    /// One stream (and the rotations that replace it in time). Returns the outcome and whether
    /// a stream lasted `stable_stream` (the backoff resets).
    async fn attempt(
        &self,
        record: &AccountRecord,
        last_id: &mut Option<u64>,
        in_tx: &mpsc::Sender<Inbound>,
        egress: &CancellationToken,
    ) -> (Outcome, bool) {
        let resumed = last_id.is_some();
        let mut stream = match self.open(record, *last_id, egress).await {
            Ok(s) => s,
            Err(o) => return (o, false),
        };
        Stats::inc(&self.stats.sessions_started);
        let session = self
            .stats
            .sessions_started
            .load(std::sync::atomic::Ordering::Relaxed);
        self.set_state(LinkState::Ready { session });
        let api = Api {
            addr: record.address().clone(),
            token: record.token().clone(),
        };
        if !self
            .forward(in_tx, Inbound::Up { api, resumed }, egress)
            .await
        {
            return (self.not_delivered(in_tx, egress), false);
        }
        // Only a fresh stream's id is a resume point here: on a failed resume the `resync` that
        // follows carries the same id, and committing it first could skip that `resync`.
        if !resumed && let Some(id) = stream.ready_id {
            *last_id = Some(id);
        }
        let mut stable = false;
        let outcome = loop {
            let end = self.read(&mut stream, last_id, in_tx, egress).await;
            let lasted = self.clock.now().saturating_since(stream.opened);
            stable |= lasted >= self.cfg.stable_stream;
            match end {
                ReadEnd::Eof if lasted >= self.cfg.stable_stream => {}
                // EOF of a short stream: not a rotation, back off.
                ReadEnd::Eof => break Outcome::Ended,
                ReadEnd::Other(o) => break o,
            }
            // A planned rotation: the old connection goes first (one stream per device).
            drop(stream);
            stream = match deadline(
                &*self.clock,
                self.cfg.rotation_grace,
                self.open(record, *last_id, egress),
            )
            .await
            {
                Some(Ok(s)) => s,
                Some(Err(o)) => break o,
                None => break Outcome::Ended,
            };
            Stats::inc(&self.stats.stream_rotations);
        };
        Stats::inc(&self.stats.sessions_ended);
        if !matches!(outcome, Outcome::Shutdown | Outcome::EngineGone)
            && !self.forward(in_tx, Inbound::Down, egress).await
        {
            // An engine merely busy past the deadline misses this Down; its next Up resets it.
            // Only shutdown or a dead engine end the supervisor.
            let why = self.not_delivered(in_tx, egress);
            if !matches!(why, Outcome::Ended) {
                return (why, stable);
            }
        }
        (outcome, stable)
    }

    /// Opens `/events` (resuming after `last_id`) and waits for its `ready`.
    async fn open(
        &self,
        record: &AccountRecord,
        last_id: Option<u64>,
        egress: &CancellationToken,
    ) -> Result<Stream<H::Body>, Outcome> {
        let clock = &*self.clock;
        let mut account = self.account.clone();
        let resume = last_id.map(|n| n.to_string());
        let call = HttpCall::Events { audio: false };
        let open = self.http.open_stream(
            record.address(),
            &call,
            record.token(),
            resume.as_deref(),
            None,
        );
        let reply = tokio::select! {
            biased;
            () = egress.cancelled() => return Err(Outcome::Shutdown),
            _ = account.changed() => return Err(Outcome::AccountChanged),
            r = deadline(clock, self.cfg.connect_timeout, open) => r,
        };
        let Some(Ok(reply)) = reply else {
            return Err(Outcome::ConnectFailed);
        };
        let opened = clock.now();
        let mut body = reply.body;
        if !(200..300).contains(&reply.status) {
            let read = read_capped(clock, &mut body, MAX_BODY_BYTES, self.cfg.http_timeout);
            let bytes = tokio::select! {
                biased;
                () = egress.cancelled() => return Err(Outcome::Shutdown),
                b = read => b,
            };
            return Err(match classify_http(reply.status, bytes.as_deref()) {
                HttpClass::Revoked => Outcome::Revoked,
                HttpClass::MissingScope => Outcome::Park(CloseAction::MissingScope),
                // A proxy's 404 while the gateway restarts must not park the daemon for good:
                // anything but 401/403 backs off (capped, jittered).
                HttpClass::Fatal | HttpClass::Ok | HttpClass::Retry => Outcome::ConnectFailed,
            });
        }
        let ready_by = opened.saturating_add(self.cfg.ready_timeout);
        let mut reader = SseReader::new();
        let mut items = Vec::new();
        loop {
            // `chunk()` is cancel-safe (port contract): a valid looping branch.
            let chunk = tokio::select! {
                biased;
                () = egress.cancelled() => return Err(Outcome::Shutdown),
                _ = account.changed() => return Err(Outcome::AccountChanged),
                c = body.chunk() => c,
                () = clock.sleep_until(ready_by) => return Err(Outcome::ConnectFailed),
            };
            let Ok(Some(bytes)) = chunk else {
                return Err(Outcome::ConnectFailed);
            };
            collect(&mut reader, &bytes, &mut items);
            // Before `ready` only `ready` counts.
            let Some(pos) = items.iter().position(|i| {
                matches!(
                    i,
                    Item::Ev {
                        ev: Event::Ready { .. },
                        ..
                    }
                )
            }) else {
                items.clear();
                continue;
            };
            let rest = items.split_off(pos + 1);
            let Some(Item::Ev {
                ev: Event::Ready { protocol, .. },
                id,
                ..
            }) = items.pop()
            else {
                return Err(Outcome::ConnectFailed);
            };
            if protocol.is_some_and(|p| p != PROTOCOL_VERSION) {
                return Err(Outcome::Park(CloseAction::UpgradeClient));
            }
            return Ok(Stream {
                body,
                reader,
                opened,
                ready_id: id,
                rest,
            });
        }
    }

    /// Reads one stream until it ends, delivering every item.
    async fn read(
        &self,
        s: &mut Stream<H::Body>,
        last_id: &mut Option<u64>,
        in_tx: &mpsc::Sender<Inbound>,
        egress: &CancellationToken,
    ) -> ReadEnd {
        let clock = &*self.clock;
        let mut account = self.account.clone();
        let mut items = std::mem::take(&mut s.rest);
        loop {
            for item in items.drain(..) {
                if !self.deliver(item, last_id, in_tx, egress).await {
                    return ReadEnd::Other(self.not_delivered(in_tx, egress));
                }
            }
            let chunk = tokio::select! {
                biased;
                () = egress.cancelled() => return ReadEnd::Other(Outcome::Shutdown),
                _ = account.changed() => return ReadEnd::Other(Outcome::AccountChanged),
                c = deadline(clock, self.cfg.events_read_timeout, s.body.chunk()) => c,
            };
            match chunk {
                Some(Ok(Some(bytes))) => collect(&mut s.reader, &bytes, &mut items),
                Some(Ok(None)) => return ReadEnd::Eof,
                // Silent past the read timeout (dead peer, half-open) or broken.
                None | Some(Err(_)) => return ReadEnd::Other(Outcome::Ended),
            }
        }
    }

    /// Hands one item to the engine (must-deliver) and then moves the resume point. `false` =
    /// not delivered (shutdown, engine gone or stalled).
    async fn deliver(
        &self,
        item: Item,
        last_id: &mut Option<u64>,
        in_tx: &mpsc::Sender<Inbound>,
        egress: &CancellationToken,
    ) -> bool {
        let (ev, id, bytes) = match item {
            Item::Oversize(id) => {
                Stats::inc(&self.stats.events_oversize);
                if !self.forward(in_tx, Inbound::Resync, egress).await {
                    return false;
                }
                Stats::inc(&self.stats.events_resync);
                // Resume past it: the resync re-fetched what it carried, and the gateway would
                // replay the same too-big event at every rotation.
                if let Some(id) = id
                    && last_id.is_none_or(|l| id > l)
                {
                    *last_id = Some(id);
                }
                return true;
            }
            Item::Ev { ev, id, bytes } => (ev, id, bytes),
        };
        match ev {
            // Only a stream's first `ready` counts (handled by `open`).
            Event::Ready { .. } => return true,
            Event::Audio => Stats::inc(&self.stats.events_audio_dropped),
            Event::Unknown => Stats::inc(&self.stats.inbound_invalid),
            Event::Resync => {
                if !self.forward(in_tx, Inbound::Resync, egress).await {
                    return false;
                }
                Stats::inc(&self.stats.events_resync);
            }
            ev => {
                let Some(charge) = self.charge(bytes, egress).await else {
                    return false;
                };
                if !self
                    .forward(
                        in_tx,
                        Inbound::Event {
                            ev,
                            id,
                            bytes,
                            charge,
                        },
                        egress,
                    )
                    .await
                {
                    return false;
                }
                Stats::inc(&self.stats.events_received);
            }
        }
        if id.is_some() {
            *last_id = id;
        }
        true
    }

    /// Why a must-deliver enqueue failed: shutdown, the engine is gone, or (the engine is only
    /// busy past `inbound_deadline`) the stream ends and resumes after a backoff.
    fn not_delivered(&self, in_tx: &mpsc::Sender<Inbound>, egress: &CancellationToken) -> Outcome {
        if egress.is_cancelled() {
            Outcome::Shutdown
        } else if in_tx.is_closed() {
            Outcome::EngineGone
        } else {
            Outcome::Ended
        }
    }

    /// Charges an event's bytes against `INBOUND_BYTES`, waiting up to `inbound_deadline`.
    /// `None` = shutdown or the engine stalled past the deadline.
    async fn charge(&self, bytes: usize, egress: &CancellationToken) -> Option<Charge> {
        tokio::select! {
            biased;
            () = egress.cancelled() => None,
            c = deadline(&*self.clock, self.cfg.inbound_deadline, self.budget.charge(bytes)) => c.flatten(),
        }
    }

    /// Must-deliver enqueue with a deadline (see `queues`). `false` = shutdown, engine gone, or
    /// the engine stalled past the deadline.
    async fn forward(
        &self,
        in_tx: &mpsc::Sender<Inbound>,
        ev: Inbound,
        egress: &CancellationToken,
    ) -> bool {
        let permit = tokio::select! {
            biased;
            () = egress.cancelled() => return false,
            p = deadline(&*self.clock, self.cfg.inbound_deadline, in_tx.reserve()) => p,
        };
        match permit {
            Some(Ok(p)) => {
                p.send(ev);
                Stats::hwm(
                    &self.stats.in_queue_hwm,
                    in_tx.max_capacity() - in_tx.capacity(),
                );
                true
            }
            Some(Err(_)) | None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::queues::{INBOUND, INBOUND_BYTES};
    use fogar_ports::secrets::SecretKey;
    use fogar_testkit::{
        EventsHub, FakeGatewayHttp, HttpRoutes, MemorySecretStore, SeededRandom, TestClock,
    };

    #[test]
    fn statuses_are_classified() {
        let scope = Some(br#"{"detail":"missing_scope"}"#.as_slice());
        let other = Some(br#"{"detail":"forbidden"}"#.as_slice());
        let auth = Some(br#"{"detail":"not_authenticated"}"#.as_slice());
        for (status, body, want) in [
            (200, None, HttpClass::Ok),
            (202, None, HttpClass::Ok),
            (401, auth, HttpClass::Revoked),
            (401, None, HttpClass::Retry),
            (401, other, HttpClass::Retry),
            (401, Some(b"<html>".as_slice()), HttpClass::Retry),
            (403, scope, HttpClass::MissingScope),
            (403, other, HttpClass::Fatal),
            (403, Some(b"<html>".as_slice()), HttpClass::Retry),
            (403, None, HttpClass::Retry),
            (404, None, HttpClass::Fatal),
            (302, None, HttpClass::Fatal),
            (429, None, HttpClass::Retry),
            (500, None, HttpClass::Retry),
            (503, other, HttpClass::Retry),
        ] {
            assert_eq!(classify_http(status, body), want, "{status}");
        }
    }

    /// A store signed in at the hub's address with `key` (through the dev-key path: archtest
    /// R14 keeps the account's item name in `account.rs`).
    fn paired_store(addr: &str, key: &str) -> MemorySecretStore {
        let s = MemorySecretStore::default();
        s.put(SecretKey::GatewayClientKey, key.to_owned().into())
            .unwrap();
        let addr = GatewayAddr::parse(addr).unwrap();
        account::adopt_stored_token(&s, addr).unwrap();
        assert!(account::load(&s).unwrap().is_some());
        s
    }

    struct Run {
        budget: ByteBudget,
        stats: Arc<Stats>,
        clock: Arc<TestClock>,
        hub: EventsHub,
        in_rx: mpsc::Receiver<Inbound>,
        egress: CancellationToken,
        tasks: tokio_util::task::TaskTracker,
        view_rx: watch::Receiver<LinkView>,
        /// Signals "the account changed" (as `Login` does).
        account: watch::Sender<()>,
    }

    /// `in_cap`: inbound queue capacity; `prefill`: messages already queued (an engine that
    /// is not draining).
    fn start(in_cap: usize, prefill: usize, secrets: Arc<dyn SecretStore>) -> Run {
        let routes = HttpRoutes::new();
        let hub = EventsHub::new();
        hub.install(&routes);
        let clock = Arc::new(TestClock::new());
        let stats = Arc::new(Stats::default());
        let budget = ByteBudget::new(INBOUND_BYTES);
        let (view, view_rx) = watch::channel(LinkView {
            state: LinkState::Starting,
            last_close: None,
            gateway: None,
        });
        let (account_keep, account) = watch::channel(());
        let (reconnect_keep, reconnect) = watch::channel(());
        let ctx = LinkCtx {
            clock: clock.clone(),
            http: Arc::new(FakeGatewayHttp::new(routes)),
            secrets,
            random: Arc::new(SeededRandom::new(7)),
            cfg: Arc::new(Config::default()),
            stats: stats.clone(),
            view,
            budget: budget.clone(),
            account,
            _account_keep: account_keep,
            reconnect,
            _reconnect_keep: reconnect_keep,
        };
        let account_tx = ctx._account_keep.clone();
        let (in_tx, in_rx) = mpsc::channel(in_cap);
        for _ in 0..prefill {
            in_tx.try_send(Inbound::Down).unwrap();
        }
        let egress = CancellationToken::new();
        let tasks = tokio_util::task::TaskTracker::new();
        tasks.spawn(ctx.run(in_tx, egress.clone()));
        tasks.close();
        Run {
            budget,
            stats,
            clock,
            hub,
            in_rx,
            egress,
            tasks,
            view_rx,
            account: account_tx,
        }
    }

    /// Review L2 (security): a `SignedOut` the stalled engine missed is not lost: it arrives
    /// before the next account's `Up`.
    #[tokio::test(start_paused = true)]
    async fn a_missed_sign_out_comes_before_the_next_up() {
        let store = Arc::new(MemorySecretStore::default());
        let mut r = start(1, 1, store.clone());
        r.clock.sleep(Duration::from_secs(30)).await; // past `inbound_deadline`: missed
        assert!(
            matches!(r.in_rx.recv().await, Some(Inbound::Down)),
            "prefill"
        );
        store
            .put(SecretKey::GatewayClientKey, "k".to_owned().into())
            .unwrap();
        account::adopt_stored_token(&*store, GatewayAddr::parse("127.0.0.1:8004").unwrap())
            .unwrap();
        r.account.send_replace(());
        assert!(matches!(r.in_rx.recv().await, Some(Inbound::SignedOut)));
        assert!(matches!(r.in_rx.recv().await, Some(Inbound::Up { .. })));
        stop(r).await;
    }

    /// PR 4a review L: an owed `SignedOut` is retried on the backoff, so an engine that recovers
    /// drops the old account without waiting for a login.
    #[tokio::test(start_paused = true)]
    async fn a_missed_sign_out_is_retried_without_a_login() {
        let mut r = start(1, 1, Arc::new(MemorySecretStore::default()));
        r.clock.sleep(Duration::from_secs(30)).await; // past `inbound_deadline`: missed
        assert!(
            matches!(r.in_rx.recv().await, Some(Inbound::Down)),
            "prefill"
        );
        // No account change: the retry timer alone delivers it (within the backoff cap).
        let got = tokio::time::timeout(Duration::from_secs(120), r.in_rx.recv()).await;
        assert!(matches!(got, Ok(Some(Inbound::SignedOut))), "{got:?}");
        // Delivered: no further timer, nothing more is sent.
        let more = tokio::time::timeout(Duration::from_secs(3600), r.in_rx.recv()).await;
        assert!(more.is_err(), "{more:?}");
        stop(r).await;
    }

    fn signed_in() -> Arc<dyn SecretStore> {
        Arc::new(paired_store("127.0.0.1:8004", "k"))
    }

    async fn stop(r: Run) {
        r.egress.cancel();
        drop(r.in_rx);
        r.tasks.wait().await;
    }

    /// Audit #2: a store this build cannot use is "not paired", not "reconnecting" forever.
    #[tokio::test(start_paused = true)]
    async fn unsupported_secret_store_is_not_paired() {
        struct Stub;
        impl SecretStore for Stub {
            fn capabilities(&self) -> fogar_ports::secrets::SecretCaps {
                fogar_ports::secrets::SecretCaps::NONE
            }
            fn get(&self, _: SecretKey) -> Result<Option<SecretString>, SecretError> {
                Err(SecretError::Unsupported("stub"))
            }
            fn put(&self, _: SecretKey, _: SecretString) -> Result<(), SecretError> {
                Err(SecretError::Unsupported("stub"))
            }
            fn delete(&self, _: SecretKey) -> Result<(), SecretError> {
                Err(SecretError::Unsupported("stub"))
            }
        }
        let r = start(INBOUND, 0, Arc::new(Stub));
        r.clock.sleep(Duration::from_secs(3600)).await;
        assert_eq!(r.view_rx.borrow().state, LinkState::NotPaired);
        assert_eq!(r.stats.snapshot().connect_attempts, 0);
        stop(r).await;
    }

    /// Audit #1: an engine busy past `inbound_deadline` exactly when `ready` arrives must not
    /// end the supervisor (nothing respawns it): the stream is dropped and the link reconnects
    /// after a backoff.
    #[tokio::test(start_paused = true)]
    async fn engine_busy_at_ready_is_not_a_shutdown() {
        let r = start(1, 1, signed_in());
        r.clock.sleep(Duration::from_secs(30)).await;
        assert_eq!(r.tasks.len(), 1, "the link supervisor must still run");
        assert!(
            !matches!(r.view_rx.borrow().state, LinkState::Ready { .. }),
            "{:?}",
            r.view_rx.borrow().state
        );
        assert!(r.stats.snapshot().connect_attempts >= 2, "reconnects");
        assert!(r.hub.subscribers() <= 1);
        stop(r).await;
    }

    /// Review round-2 F6, carried over: every queued event is charged by its bytes against
    /// `INBOUND_BYTES`, so 128 slots of 100 KiB events cannot hold 12.8 MiB.
    #[tokio::test(start_paused = true)]
    async fn events_are_charged_and_wait_for_room() {
        let mut r = start(INBOUND, 0, signed_in());
        r.hub.seed_conversation("c-1", "x");
        r.clock.sleep(Duration::from_millis(500)).await;
        assert!(matches!(r.in_rx.recv().await, Some(Inbound::Up { .. })));
        let big = "x".repeat(100 * 1024);
        for _ in 0..60 {
            r.hub.other_device_send("c-1", &big);
        }
        r.clock.sleep(Duration::from_millis(500)).await;
        let queued = r.in_rx.len();
        assert!(queued < 60, "{queued} queued: the byte budget binds first");
        assert!(r.budget.in_use() > INBOUND_BYTES - 100 * 1024 - 1024);
        // Dropping one releases its bytes: the link goes on.
        let first = r.in_rx.recv().await;
        assert!(matches!(first, Some(Inbound::Event { .. })));
        drop(first);
        r.clock.sleep(Duration::from_millis(500)).await;
        assert!(
            r.in_rx.len() >= queued,
            "the next event was queued once there was room"
        );
        stop(r).await;
    }

    #[tokio::test(start_paused = true)]
    async fn read_capped_stops_at_the_cap_and_the_deadline() {
        let clock = TestClock::new();
        let routes = HttpRoutes::new();
        let http = FakeGatewayHttp::new(routes.clone());
        let addr = http.addr();
        let t = SecretString::from("t");
        let call = HttpCall::Events { audio: false };
        routes.set("GET", "/events", 503, vec![b'x'; 100]);
        let mut b = http
            .open_stream(&addr, &call, &t, None, None)
            .await
            .unwrap()
            .body;
        assert_eq!(
            read_capped(&clock, &mut b, 100, Duration::from_secs(1))
                .await
                .map(|v| v.len()),
            Some(100)
        );
        let mut b = http
            .open_stream(&addr, &call, &t, None, None)
            .await
            .unwrap()
            .body;
        assert_eq!(
            read_capped(&clock, &mut b, 99, Duration::from_secs(1)).await,
            None
        );
        routes.set_stream(
            "GET",
            "/events",
            503,
            vec![fogar_testkit::StreamStep::HoldOpen],
        );
        let mut b = http
            .open_stream(&addr, &call, &t, None, None)
            .await
            .unwrap()
            .body;
        assert_eq!(
            read_capped(&clock, &mut b, 99, Duration::from_secs(1)).await,
            None
        );
    }
}
