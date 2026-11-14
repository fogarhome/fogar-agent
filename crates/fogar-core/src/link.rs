//! Gateway connection supervisor + session loop (fogar-longrun-reliability "WebSocket connection
//! supervisor"). Protocol v1 specifics: handshake JSON without `type`, `status` frames may precede
//! `ready`, no application ping (keepalive = `{"type":"keepalive"}` after client silence), closes
//! are classified by code + reason (spec §11).
//!
//! Ownership: ONE supervisor task owns the uplink `Receiver` across sessions; each session is run
//! inline (no per-session tasks to leak). Cancellation: the `egress` token ends the current
//! session with a close frame (under `close_timeout`) and stops reconnecting.

use crate::{
    config::Config,
    pairing::{PairingRecord, is_revocation, load_record},
    queues::{ByteBudget, Charge},
    stats::{LinkState, Stats},
    time::deadline,
};
use fogar_ports::{
    clock::{Clock, Mono},
    gateway::{Frame, GatewayLink, LinkEvent, LinkSession},
    ipc::GatewayInfo,
    random::Random,
    secrets::{ExposeSecret, SecretKey, SecretStore},
};
use fogar_proto::{
    ClientMsg, CloseAction, Handshake, ServerMsg, classify_close, close::CLOSE_ABNORMAL,
    close::CLOSE_NORMAL, decode_server, encode_client, encode_handshake,
};
use std::{borrow::Cow, sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

/// Session -> engine events. Frames carry the [`Charge`] of their bytes against
/// `queues::INBOUND_BYTES`; it is released when the engine drops the message.
pub(crate) enum Inbound {
    SessionUp,
    SessionDown,
    Server(ServerMsg, Charge),
    /// Raw binary frame (0xA1 + turn_seq + PCM16); the engine parses it zero-copy.
    Audio(Vec<u8>, Charge),
}

/// Redacting `Debug` (review sec#7, LOG-1): transcripts, tokens and audio never reach a log line.
impl std::fmt::Debug for Inbound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Inbound::SessionUp => f.write_str("SessionUp"),
            Inbound::SessionDown => f.write_str("SessionDown"),
            Inbound::Server(m, _) => f.debug_tuple("Server").field(m).finish(),
            Inbound::Audio(b, _) => write!(f, "Audio(<{} bytes>)", b.len()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkView {
    pub state: LinkState,
    pub last_close: Option<(u16, CloseAction)>,
    /// The gateway of the pairing record the link is using; `None` = not paired.
    pub gateway: Option<GatewayInfo>,
}

pub(crate) struct LinkCtx<C, L> {
    pub(crate) clock: Arc<C>,
    pub(crate) link: L,
    pub(crate) secrets: Arc<dyn SecretStore>,
    pub(crate) random: Arc<dyn Random>,
    pub(crate) cfg: Arc<Config>,
    pub(crate) stats: Arc<Stats>,
    pub(crate) view: watch::Sender<LinkView>,
    /// Byte budget of the inbound queue (`queues::INBOUND_BYTES`).
    pub(crate) budget: ByteBudget,
    /// Bumped whenever the pairing record is written or deleted: the link drops what it is
    /// doing (session, backoff, park) and starts over from the stored record.
    pub(crate) pairing: watch::Receiver<()>,
    /// Keeps the pairing channel open while the link runs: a closed `watch` would make every
    /// `changed()` branch above resolve at once (a hot loop).
    pub(crate) _pairing_keep: watch::Sender<()>,
}

/// Why a connection attempt / session ended.
#[derive(Debug)]
enum Outcome {
    Shutdown,
    /// Engine dropped its end: nothing left to serve.
    EngineGone,
    /// Transport connect / handshake write / ready wait failed before `ready`.
    ConnectFailed,
    /// Our handshake cannot be encoded (e.g. over 4096 chars): retrying cannot help.
    ClientBug,
    /// The peer closed (or the transport died: 1006).
    Closed {
        code: u16,
        reason: String,
    },
    /// We ended the session: the engine stalled or a write failed / timed out.
    LocalFault,
    /// The pairing record changed (paired, unpaired): start over without backoff.
    PairingChanged,
}

impl<C: Clock, L: GatewayLink> LinkCtx<C, L> {
    fn set_state(&self, state: LinkState) {
        self.view.send_modify(|v| v.state = state);
    }

    /// `false` = shutdown. Returns early (`true`) when the pairing record changes.
    async fn wait(&self, d: Duration, egress: &CancellationToken) -> bool {
        let mut pairing = self.pairing.clone();
        tokio::select! {
            biased;
            () = egress.cancelled() => false,
            _ = pairing.changed() => true,
            () = self.clock.sleep(d) => true,
        }
    }

    pub(crate) async fn run(
        mut self,
        mut out_rx: mpsc::Receiver<Frame>,
        in_tx: mpsc::Sender<Inbound>,
        egress: CancellationToken,
    ) {
        let mut attempt: u32 = 0;
        loop {
            if egress.is_cancelled() || in_tx.is_closed() {
                break;
            }
            // Changes from here on interrupt whatever the link does with this record.
            self.pairing.borrow_and_update();
            let record = match load_record(&*self.secrets) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    self.view.send_modify(|v| {
                        v.state = LinkState::NotPaired;
                        v.gateway = None;
                    });
                    // No timer: the only writers are `Pair` (signals the watch) and the CLI dev
                    // path at boot. An unpaired Mac costs no wakeups and no Keychain reads.
                    let mut pairing = self.pairing.clone();
                    tokio::select! {
                        biased;
                        () = egress.cancelled() => break,
                        r = pairing.changed() => if r.is_ok() { continue } else { break },
                    }
                }
                Err(_) => {
                    let d = self.cfg.backoff.delay(attempt, &*self.random);
                    attempt = attempt.saturating_add(1);
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
            let info = record.info();
            self.view.send_modify(|v| {
                v.state = LinkState::Connecting { attempt };
                if v.gateway.as_ref() != Some(&info) {
                    v.gateway = Some(info);
                }
            });
            Stats::inc(&self.stats.connect_attempts);
            let started = self.clock.now();
            let (outcome, was_ready) = self.attempt(&record, &mut out_rx, &in_tx, &egress).await;
            if was_ready {
                Stats::inc(&self.stats.sessions_ended);
                let up_for = self.clock.now().saturating_since(started);
                if up_for >= self.cfg.backoff.stable_after {
                    attempt = 0;
                }
                if !self.forward(&in_tx, Inbound::SessionDown, &egress).await {
                    break;
                }
            }
            let (delay, close) = match outcome {
                Outcome::Shutdown | Outcome::EngineGone => break,
                Outcome::PairingChanged => {
                    attempt = 0;
                    continue;
                }
                Outcome::ClientBug => {
                    if self.park(CloseAction::FixClient, &egress).await {
                        continue;
                    }
                    break;
                }
                Outcome::ConnectFailed | Outcome::LocalFault => {
                    if !was_ready {
                        Stats::inc(&self.stats.connect_failures);
                    }
                    let d = self.cfg.backoff.delay(attempt, &*self.random);
                    attempt = attempt.saturating_add(1);
                    (d, None)
                }
                Outcome::Closed { code, reason } => {
                    let action = classify_close(code, &reason);
                    self.view
                        .send_modify(|v| v.last_close = Some((code, action)));
                    if !was_ready {
                        Stats::inc(&self.stats.connect_failures);
                    }
                    if is_revocation(code, &reason) && self.revoke(&record) {
                        // Back to "not paired"; never retry this key. Still through the backoff:
                        // a store whose delete silently fails must not turn a gateway that
                        // revokes every connect into back-to-back reconnects (NET-4).
                        self.view.send_modify(|v| {
                            v.state = LinkState::NotPaired;
                            v.gateway = None;
                        });
                        let d = self.cfg.backoff.delay(attempt, &*self.random);
                        attempt = attempt.saturating_add(1);
                        if self.wait(d, &egress).await {
                            continue;
                        }
                        break;
                    }
                    match action {
                        // Repair without one of the two exact texts keeps the key (spec §0).
                        CloseAction::Repair
                        | CloseAction::AgentRefused
                        | CloseAction::FixClient => {
                            if self.park(action, &egress).await {
                                attempt = 0;
                                continue;
                            }
                            break;
                        }
                        // Idle close (300 s without client messages), a normal close or a
                        // server restart: not an error, but only a session that stayed up for
                        // `stable_after` has reset `attempt` above, so a gateway that closes
                        // every session at once escalates the backoff like any other failure
                        // (review code#1/sec#2: no reconnect storm, NET-4). After a stable
                        // session this is `delay(0)`: prompt, jittered reconnect.
                        CloseAction::IdleOrNormal
                        | CloseAction::ServerRestart
                        | CloseAction::RetryBackoff
                        | CloseAction::Unknown => {
                            let d = self.cfg.backoff.delay(attempt, &*self.random);
                            attempt = attempt.saturating_add(1);
                            (d, Some(action))
                        }
                    }
                }
            };
            self.set_state(LinkState::Backoff {
                attempt,
                last_close: close,
            });
            if !self.wait(delay, &egress).await {
                break;
            }
        }
    }

    /// Stays alive but stops retrying (retrying would loop: fogar-longrun "config/auth errors do
    /// not exit"). `true` = the pairing record changed (re-paired/unpaired): start over;
    /// `false` = shutdown.
    async fn park(&self, reason: CloseAction, egress: &CancellationToken) -> bool {
        self.set_state(LinkState::Parked { reason });
        let mut pairing = self.pairing.clone();
        tokio::select! {
            biased;
            () = egress.cancelled() => false,
            r = pairing.changed() => r.is_ok(),
        }
    }

    /// The gateway revoked `record`'s key: delete it, unless the record was replaced meanwhile
    /// (an Unpair + Pair raced the close). `false` = the delete failed: the caller parks rather
    /// than retry a dead key (also when the store cannot be read).
    /// Check-then-delete without the pairing lock: safe because nothing awaits in between and
    /// `Pair`/`Unpair` write the store synchronously too (one runtime thread in fogar-agentd).
    fn revoke(&self, record: &PairingRecord) -> bool {
        let Ok(stored) = load_record(&*self.secrets) else {
            return false;
        };
        let still_ours = matches!(
            stored,
            Some(r) if r.client_id() == record.client_id()
                && r.address() == record.address()
                // Both keys are our own copies (no remote timing oracle, SEC-4 n/a): a key
                // rotated under the same client_id must not be deleted by the old key's close.
                && r.client_key().expose_secret() == record.client_key().expose_secret()
        );
        if still_ours && self.secrets.delete(SecretKey::GatewayPairing).is_err() {
            return false;
        }
        true
    }

    /// One connect + handshake + session. Returns the outcome and whether `ready` was reached.
    async fn attempt(
        &self,
        record: &PairingRecord,
        out_rx: &mut mpsc::Receiver<Frame>,
        in_tx: &mpsc::Sender<Inbound>,
        egress: &CancellationToken,
    ) -> (Outcome, bool) {
        let clock = &*self.clock;
        let mut pairing = self.pairing.clone();
        let connect = tokio::select! {
            biased;
            () = egress.cancelled() => return (Outcome::Shutdown, false),
            _ = pairing.changed() => return (Outcome::PairingChanged, false),
            r = deadline(clock, self.cfg.connect_timeout, self.link.connect(record.address())) => r,
        };
        let Some(Ok(mut sess)) = connect else {
            return (Outcome::ConnectFailed, false);
        };
        let hs = Handshake {
            client_key: Cow::Borrowed(record.client_key().expose_secret()),
            input_mode: self.cfg.input_mode,
            output_mode: self.cfg.output_mode.clone(),
            agent: self.cfg.agent.as_deref().map(Cow::Borrowed),
        };
        let Ok(text) = encode_handshake(&hs) else {
            return (Outcome::ClientBug, false);
        };
        drop(hs);
        if !matches!(
            deadline(clock, self.cfg.write_timeout, sess.send(Frame::Text(text))).await,
            Some(Ok(()))
        ) {
            self.close_quietly(&mut sess, CLOSE_NORMAL).await;
            return (Outcome::ConnectFailed, false);
        }
        // Frames queued for a previous session belong to a turn that no longer exists.
        while out_rx.try_recv().is_ok() {
            Stats::inc(&self.stats.stale_outbound_discarded);
        }
        match self.await_ready(&mut sess, in_tx, egress).await {
            Ok(()) => {}
            Err(o) => return (o, false),
        }
        Stats::inc(&self.stats.sessions_started);
        let session = self
            .stats
            .sessions_started
            .load(std::sync::atomic::Ordering::Relaxed);
        self.set_state(LinkState::Ready { session });
        if !self.forward(in_tx, Inbound::SessionUp, egress).await {
            self.close_quietly(&mut sess, CLOSE_NORMAL).await;
            return (Outcome::Shutdown, true);
        }
        (self.session(&mut sess, out_rx, in_tx, egress).await, true)
    }

    /// Waits for `ready`, tolerating (and forwarding) `status` frames before it (spec §5).
    async fn await_ready(
        &self,
        sess: &mut L::Session,
        in_tx: &mpsc::Sender<Inbound>,
        egress: &CancellationToken,
    ) -> Result<(), Outcome> {
        let clock = &*self.clock;
        let ready_by = clock.now().saturating_add(self.cfg.ready_timeout);
        let mut pairing = self.pairing.clone();
        loop {
            tokio::select! {
                biased;
                () = egress.cancelled() => {
                    self.close_quietly(sess, CLOSE_NORMAL).await;
                    return Err(Outcome::Shutdown);
                }
                _ = pairing.changed() => {
                    self.close_quietly(sess, CLOSE_NORMAL).await;
                    return Err(Outcome::PairingChanged);
                }
                ev = sess.recv() => match ev {
                    None => return Err(Outcome::Closed { code: CLOSE_ABNORMAL, reason: String::new() }),
                    Some(LinkEvent::Closed { code, reason }) => return Err(Outcome::Closed { code, reason }),
                    Some(LinkEvent::Frame(Frame::Text(t))) => match decode_server(&t) {
                        Ok(ServerMsg::Ready(_)) => return Ok(()),
                        Ok(msg @ ServerMsg::Status(_)) => {
                            let Some(charge) = self.charge(t.len(), egress).await else {
                                return Err(Outcome::EngineGone);
                            };
                            if !self.forward(in_tx, Inbound::Server(msg, charge), egress).await {
                                return Err(Outcome::EngineGone);
                            }
                        }
                        Ok(_) => {}
                        Err(_) => Stats::inc(&self.stats.inbound_invalid),
                    },
                    Some(LinkEvent::Frame(Frame::Binary(_))) => {}
                },
                () = clock.sleep_until(ready_by) => {
                    self.close_quietly(sess, CLOSE_NORMAL).await;
                    return Err(Outcome::ConnectFailed);
                }
            }
        }
    }

    async fn session(
        &self,
        sess: &mut L::Session,
        out_rx: &mut mpsc::Receiver<Frame>,
        in_tx: &mpsc::Sender<Inbound>,
        egress: &CancellationToken,
    ) -> Outcome {
        let clock = &*self.clock;
        let mut last_sent: Mono = clock.now();
        let mut pairing = self.pairing.clone();
        loop {
            // One armed timer at most (energy rule 1): the keepalive deadline, pushed forward by
            // every frame we send.
            let keepalive_due = self
                .cfg
                .keepalive_after
                .map(|d| last_sent.saturating_add(d));
            tokio::select! {
                biased;
                () = egress.cancelled() => {
                    self.close_quietly(sess, CLOSE_NORMAL).await;
                    return Outcome::Shutdown;
                }
                _ = pairing.changed() => {
                    self.close_quietly(sess, CLOSE_NORMAL).await;
                    return Outcome::PairingChanged;
                }
                ev = sess.recv() => match ev {
                    None => return Outcome::Closed { code: CLOSE_ABNORMAL, reason: String::new() },
                    Some(LinkEvent::Closed { code, reason }) => return Outcome::Closed { code, reason },
                    Some(LinkEvent::Frame(Frame::Text(t))) => match decode_server(&t) {
                        Ok(ServerMsg::Unknown) => {}
                        Ok(msg) => {
                            let delivered = self
                                .deliver_draining(sess, out_rx, in_tx, msg, t.len(), egress, &mut last_sent)
                                .await;
                            if !delivered {
                                if egress.is_cancelled() {
                                    self.close_quietly(sess, CLOSE_NORMAL).await;
                                    return Outcome::Shutdown;
                                }
                                if in_tx.is_closed() {
                                    self.close_quietly(sess, CLOSE_NORMAL).await;
                                    return Outcome::EngineGone;
                                }
                                self.close_quietly(sess, CLOSE_NORMAL).await;
                                return Outcome::LocalFault;
                            }
                        }
                        Err(_) => Stats::inc(&self.stats.inbound_invalid),
                    },
                    Some(LinkEvent::Frame(Frame::Binary(b))) => match self.budget.try_charge(b.len()) {
                        None => Stats::inc(&self.stats.inbound_audio_shed),
                        Some(charge) => match in_tx.try_send(Inbound::Audio(b, charge)) {
                        Ok(()) => Stats::hwm(&self.stats.in_queue_hwm, in_tx.max_capacity() - in_tx.capacity()),
                        Err(mpsc::error::TrySendError::Full(_)) => Stats::inc(&self.stats.inbound_audio_shed),
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            self.close_quietly(sess, CLOSE_NORMAL).await;
                            return Outcome::EngineGone;
                        }
                        },
                    },
                },
                out = out_rx.recv() => match out {
                    None => {
                        self.close_quietly(sess, CLOSE_NORMAL).await;
                        return Outcome::EngineGone;
                    }
                    Some(frame) => {
                        if !matches!(deadline(clock, self.cfg.write_timeout, sess.send(frame)).await, Some(Ok(()))) {
                            self.close_quietly(sess, CLOSE_NORMAL).await;
                            return Outcome::LocalFault;
                        }
                        Stats::inc(&self.stats.frames_sent);
                        last_sent = clock.now();
                    }
                },
                () = sleep_until_opt(clock, keepalive_due), if keepalive_due.is_some() => {
                    let Ok(text) = encode_client(&ClientMsg::Keepalive) else {
                        return Outcome::ClientBug;
                    };
                    if !matches!(deadline(clock, self.cfg.write_timeout, sess.send(Frame::Text(text))).await, Some(Ok(()))) {
                        self.close_quietly(sess, CLOSE_NORMAL).await;
                        return Outcome::LocalFault;
                    }
                    Stats::inc(&self.stats.keepalives_sent);
                    last_sent = clock.now();
                }
            }
        }
    }

    async fn close_quietly(&self, sess: &mut L::Session, code: u16) {
        let _ = deadline(&*self.clock, self.cfg.close_timeout, sess.close(code, "")).await;
    }
}

async fn sleep_until_opt<C: Clock>(clock: &C, at: Option<Mono>) {
    match at {
        Some(t) => clock.sleep_until(t).await,
        None => std::future::pending().await,
    }
}

impl<C: Clock, L: GatewayLink> LinkCtx<C, L> {
    /// Must-deliver enqueue of one JSON event (byte charge + queue slot, each under
    /// `inbound_deadline`) that KEEPS DRAINING the uplink queue while it waits (review code#6).
    /// The link is the only reader of `out_rx`; if it waited for inbound room without draining,
    /// an engine waiting for outbound room (must-deliver control) and the link would wait on each
    /// other until both deadlines fired. `false` = not delivered (shutdown, engine gone, engine
    /// stalled, or an uplink write failed / timed out); the caller classifies and closes.
    #[allow(clippy::too_many_arguments)]
    async fn deliver_draining(
        &self,
        sess: &mut L::Session,
        out_rx: &mut mpsc::Receiver<Frame>,
        in_tx: &mpsc::Sender<Inbound>,
        msg: ServerMsg,
        bytes: usize,
        egress: &CancellationToken,
        last_sent: &mut Mono,
    ) -> bool {
        let enqueue = async {
            match self.charge(bytes, egress).await {
                Some(c) => self.forward(in_tx, Inbound::Server(msg, c), egress).await,
                None => false,
            }
        };
        tokio::pin!(enqueue);
        let mut out_open = true;
        loop {
            tokio::select! {
                biased;
                ok = &mut enqueue => return ok,
                out = out_rx.recv(), if out_open => match out {
                    // Engine gone: the enqueue notices (closed channel) and returns false.
                    None => out_open = false,
                    Some(frame) => {
                        if !matches!(deadline(&*self.clock, self.cfg.write_timeout, sess.send(frame)).await, Some(Ok(()))) {
                            return false;
                        }
                        Stats::inc(&self.stats.frames_sent);
                        *last_sent = self.clock.now();
                    }
                },
            }
        }
    }

    /// Charges a must-deliver JSON frame's bytes against `INBOUND_BYTES`, waiting up to
    /// `inbound_deadline`. `None` = shutdown or the engine stalled past the deadline.
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

/// Round-2 review F6: the link must charge every queued inbound frame (TTS audio and JSON)
/// against `INBOUND_BYTES`. These tests drive `LinkCtx::run` with an engine that never consumes
/// and read the shared budget directly; a link that charged 0 (or 1) byte per frame would let
/// the queue hold 128 x 1 MiB.
#[cfg(test)]
mod budget_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::queues::{INBOUND, INBOUND_BYTES};
    use fogar_ports::gateway::LinkError;
    use fogar_testkit::{MemorySecretStore, SeededRandom, TestClock};

    fn paired_store(addr: &str, key: &str) -> MemorySecretStore {
        let s = MemorySecretStore::default();
        let rec = PairingRecord::new(
            fogar_ports::gateway::GatewayAddr::parse(addr).unwrap(),
            "c-1".into(),
            key.to_owned().into(),
            "gw".into(),
            "1".into(),
            "Mac".into(),
        );
        s.put(SecretKey::GatewayPairing, rec.encode()).unwrap();
        s
    }
    use std::{collections::VecDeque, sync::Mutex};

    const READY: &str = r#"{"type":"ready","session_id":"c:1"}"#;

    /// One session: `ready`, then the scripted frames, then silence.
    #[derive(Clone)]
    struct Scripted(Arc<Mutex<Option<VecDeque<Frame>>>>);
    struct Sess(VecDeque<Frame>);

    impl LinkSession for Sess {
        fn max_frame(&self) -> usize {
            fogar_proto::limits::WS_MAX_FRAME_BYTES
        }
        async fn send(&mut self, _: Frame) -> Result<(), LinkError> {
            Ok(())
        }
        async fn recv(&mut self) -> Option<LinkEvent> {
            match self.0.pop_front() {
                Some(f) => Some(LinkEvent::Frame(f)),
                None => std::future::pending().await,
            }
        }
        async fn close(&mut self, _: u16, _: &str) -> Result<(), LinkError> {
            Ok(())
        }
    }

    impl GatewayLink for Scripted {
        type Session = Sess;
        async fn connect(&self, _: &fogar_ports::gateway::GatewayAddr) -> Result<Sess, LinkError> {
            let taken = self.0.lock().unwrap().take();
            match taken {
                Some(frames) => Ok(Sess(frames)),
                None => std::future::pending().await,
            }
        }
    }

    struct Run {
        budget: ByteBudget,
        stats: Arc<Stats>,
        clock: Arc<TestClock>,
        in_rx: mpsc::Receiver<Inbound>,
        egress: CancellationToken,
        tasks: tokio_util::task::TaskTracker,
        /// Held so the uplink stays open (no `EngineGone`).
        _out_tx: mpsc::Sender<Frame>,
    }

    fn start(frames: Vec<Frame>) -> Run {
        let mut script: VecDeque<Frame> = VecDeque::new();
        script.push_back(Frame::Text(READY.into()));
        script.extend(frames);
        let clock = Arc::new(TestClock::new());
        let stats = Arc::new(Stats::default());
        let budget = ByteBudget::new(INBOUND_BYTES);
        let (view, _) = watch::channel(LinkView {
            state: LinkState::Starting,
            last_close: None,
            gateway: None,
        });
        let (pairing_keep, pairing) = watch::channel(());
        let ctx = LinkCtx {
            clock: clock.clone(),
            link: Scripted(Arc::new(Mutex::new(Some(script)))),
            secrets: Arc::new(paired_store("127.0.0.1", "k")),
            random: Arc::new(SeededRandom::new(7)),
            cfg: Arc::new(Config::default()),
            stats: stats.clone(),
            view,
            budget: budget.clone(),
            pairing,
            _pairing_keep: pairing_keep,
        };
        let (out_tx, out_rx) = mpsc::channel(8);
        let (in_tx, in_rx) = mpsc::channel(INBOUND);
        let egress = CancellationToken::new();
        let tasks = tokio_util::task::TaskTracker::new();
        tasks.spawn(ctx.run(out_rx, in_tx, egress.clone()));
        tasks.close();
        Run {
            budget,
            stats,
            clock,
            in_rx,
            egress,
            tasks,
            _out_tx: out_tx,
        }
    }

    async fn settle(r: &Run) {
        // Well below `inbound_deadline` (5 s): the link has done all it can without waiting.
        r.clock.sleep(Duration::from_millis(500)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn audio_frames_are_charged_and_shed_when_the_budget_is_full() {
        let mib = 1 << 20;
        let r = start((0..10).map(|_| Frame::Binary(vec![0xA1; mib])).collect());
        settle(&r).await;
        assert_eq!(
            r.budget.in_use(),
            INBOUND_BYTES,
            "4 x 1 MiB queued and charged"
        );
        assert_eq!(r.stats.snapshot().inbound_audio_shed, 6, "the rest is shed");
        let queued = r.in_rx.len();
        assert_eq!(queued, 1 + 4, "SessionUp + 4 audio frames");
        r.egress.cancel();
        drop(r.in_rx);
        r.tasks.wait().await;
    }

    #[tokio::test(start_paused = true)]
    async fn json_frames_are_charged_and_wait_for_room() {
        // Just under 1 MiB each: 4 fit into the 4 MiB budget, the 5th must wait.
        let text = |i: usize| {
            let head = format!(r#"{{"type":"token","turn_id":"t-{i}","text":""#);
            let n = (1 << 20) - head.len() - 100;
            format!("{head}{}\"}}", "x".repeat(n))
        };
        let frames: Vec<String> = (0..6).map(text).collect();
        let len = frames[0].len();
        assert!(5 * len > INBOUND_BYTES && 4 * len < INBOUND_BYTES);
        let mut r = start(frames.into_iter().map(Frame::Text).collect());
        settle(&r).await;
        // 4 frames charged by their length; the waiting 5th already holds the remainder (tokio's
        // semaphore hands permits to the queued waiter as they come free).
        assert_eq!(r.budget.in_use(), INBOUND_BYTES);
        assert_eq!(r.in_rx.len(), 1 + 4, "the 5th waits for room");
        assert!(matches!(r.in_rx.recv().await, Some(Inbound::SessionUp)));
        let first = r.in_rx.recv().await;
        assert!(matches!(first, Some(Inbound::Server(..))));
        settle(&r).await;
        assert_eq!(r.in_rx.len(), 3, "a dequeued frame still holds its charge");
        drop(first); // releases `len` bytes: now the 5th fits
        settle(&r).await;
        assert_eq!(
            r.in_rx.len(),
            4,
            "the 5th was queued once its bytes were free"
        );
        assert_eq!(
            r.budget.in_use(),
            INBOUND_BYTES,
            "and the 6th waits in turn"
        );
        r.egress.cancel();
        drop(r.in_rx);
        r.tasks.wait().await;
    }
}
