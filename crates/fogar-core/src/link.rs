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
    stats::{LinkState, Stats},
    time::deadline,
};
use fogar_ports::{
    clock::{Clock, Mono},
    gateway::{Frame, GatewayLink, LinkEvent, LinkSession},
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

/// Session -> engine events.
#[derive(Debug)]
pub(crate) enum Inbound {
    SessionUp,
    SessionDown,
    Server(ServerMsg),
    /// Raw binary frame (0xA1 + turn_seq + PCM16); the engine parses it zero-copy.
    Audio(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkView {
    pub state: LinkState,
    pub last_close: Option<(u16, CloseAction)>,
}

pub(crate) struct LinkCtx<C, L> {
    pub(crate) clock: Arc<C>,
    pub(crate) link: L,
    pub(crate) secrets: Arc<dyn SecretStore>,
    pub(crate) random: Arc<dyn Random>,
    pub(crate) cfg: Arc<Config>,
    pub(crate) stats: Arc<Stats>,
    pub(crate) view: watch::Sender<LinkView>,
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
}

impl<C: Clock, L: GatewayLink> LinkCtx<C, L> {
    fn set_state(&self, state: LinkState) {
        self.view.send_modify(|v| v.state = state);
    }

    async fn wait(&self, d: Duration, egress: &CancellationToken) -> bool {
        tokio::select! {
            biased;
            () = egress.cancelled() => false,
            () = self.clock.sleep(d) => true,
        }
    }

    pub(crate) async fn run(
        self,
        mut out_rx: mpsc::Receiver<Frame>,
        in_tx: mpsc::Sender<Inbound>,
        egress: CancellationToken,
    ) {
        let mut attempt: u32 = 0;
        loop {
            if egress.is_cancelled() || in_tx.is_closed() {
                break;
            }
            let key = match self.secrets.get(SecretKey::GatewayClientKey) {
                Ok(Some(k)) => k,
                Ok(None) => {
                    self.set_state(LinkState::NotPaired);
                    if self.wait(self.cfg.backoff.cap, &egress).await {
                        continue;
                    }
                    break;
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
            self.set_state(LinkState::Connecting { attempt });
            Stats::inc(&self.stats.connect_attempts);
            let started = self.clock.now();
            let (outcome, was_ready) = self
                .attempt(key.expose_secret(), &mut out_rx, &in_tx, &egress)
                .await;
            drop(key);
            if was_ready {
                Stats::inc(&self.stats.sessions_ended);
                let up_for = self.clock.now().saturating_since(started);
                if up_for >= self.cfg.backoff.stable_after {
                    attempt = 0;
                }
                if !forward(
                    &*self.clock,
                    &self.stats,
                    &in_tx,
                    Inbound::SessionDown,
                    self.cfg.inbound_deadline,
                    &egress,
                )
                .await
                {
                    break;
                }
            }
            let (delay, close) = match outcome {
                Outcome::Shutdown | Outcome::EngineGone => break,
                Outcome::ClientBug => {
                    self.park(CloseAction::FixClient, &egress).await;
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
                    match action {
                        CloseAction::Repair
                        | CloseAction::AgentRefused
                        | CloseAction::FixClient => {
                            self.park(action, &egress).await;
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
    /// not exit"). Leaves only on shutdown in slice 0; S1 adds re-pairing / config reload.
    async fn park(&self, reason: CloseAction, egress: &CancellationToken) {
        self.set_state(LinkState::Parked { reason });
        egress.cancelled().await;
    }

    /// One connect + handshake + session. Returns the outcome and whether `ready` was reached.
    async fn attempt(
        &self,
        key: &str,
        out_rx: &mut mpsc::Receiver<Frame>,
        in_tx: &mpsc::Sender<Inbound>,
        egress: &CancellationToken,
    ) -> (Outcome, bool) {
        let clock = &*self.clock;
        let connect = tokio::select! {
            biased;
            () = egress.cancelled() => return (Outcome::Shutdown, false),
            r = deadline(clock, self.cfg.connect_timeout, self.link.connect()) => r,
        };
        let Some(Ok(mut sess)) = connect else {
            return (Outcome::ConnectFailed, false);
        };
        let hs = Handshake {
            client_key: Cow::Borrowed(key),
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
        if !forward(
            clock,
            &self.stats,
            in_tx,
            Inbound::SessionUp,
            self.cfg.inbound_deadline,
            egress,
        )
        .await
        {
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
        loop {
            tokio::select! {
                biased;
                () = egress.cancelled() => {
                    self.close_quietly(sess, CLOSE_NORMAL).await;
                    return Err(Outcome::Shutdown);
                }
                ev = sess.recv() => match ev {
                    None => return Err(Outcome::Closed { code: CLOSE_ABNORMAL, reason: String::new() }),
                    Some(LinkEvent::Closed { code, reason }) => return Err(Outcome::Closed { code, reason }),
                    Some(LinkEvent::Frame(Frame::Text(t))) => match decode_server(&t) {
                        Ok(ServerMsg::Ready(_)) => return Ok(()),
                        Ok(msg @ ServerMsg::Status(_)) => {
                            if !forward(clock, &self.stats, in_tx, Inbound::Server(msg), self.cfg.inbound_deadline, egress).await {
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
                ev = sess.recv() => match ev {
                    None => return Outcome::Closed { code: CLOSE_ABNORMAL, reason: String::new() },
                    Some(LinkEvent::Closed { code, reason }) => return Outcome::Closed { code, reason },
                    Some(LinkEvent::Frame(Frame::Text(t))) => match decode_server(&t) {
                        Ok(ServerMsg::Unknown) => {}
                        Ok(msg) => {
                            if !forward(clock, &self.stats, in_tx, Inbound::Server(msg), self.cfg.inbound_deadline, egress).await {
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
                    Some(LinkEvent::Frame(Frame::Binary(b))) => match in_tx.try_send(Inbound::Audio(b)) {
                        Ok(()) => Stats::hwm(&self.stats.in_queue_hwm, in_tx.max_capacity() - in_tx.capacity()),
                        Err(mpsc::error::TrySendError::Full(_)) => Stats::inc(&self.stats.inbound_audio_shed),
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            self.close_quietly(sess, CLOSE_NORMAL).await;
                            return Outcome::EngineGone;
                        }
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

/// Must-deliver enqueue with a deadline (see `queues`). `false` = shutdown, engine gone, or the
/// engine stalled past the deadline.
async fn forward<C: Clock>(
    clock: &C,
    stats: &Stats,
    in_tx: &mpsc::Sender<Inbound>,
    ev: Inbound,
    limit: Duration,
    egress: &CancellationToken,
) -> bool {
    let permit = tokio::select! {
        biased;
        () = egress.cancelled() => return false,
        p = deadline(clock, limit, in_tx.reserve()) => p,
    };
    match permit {
        Some(Ok(p)) => {
            p.send(ev);
            Stats::hwm(&stats.in_queue_hwm, in_tx.max_capacity() - in_tx.capacity());
            true
        }
        Some(Err(_)) | None => false,
    }
}
