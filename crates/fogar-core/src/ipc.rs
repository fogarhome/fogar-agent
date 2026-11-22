//! Local UI service over the IpcServer port (ADR-0002): version handshake, status, status
//! subscription, discovery and pairing. Connections live in a JoinSet drained in the same loop
//! and aborted on shutdown.
//!
//! Authorization (IPC-4): `Pair`/`Unpair` change where the user's voice goes and `Reconnect`
//! makes the agent spend the stored key to take the conversation back from another session (any
//! same-user process could otherwise drive the 4102 ping-pong spec §13 forbids), so they are
//! answered only for a `PeerCheck::SameSigner` peer, unless the composition root opted in to
//! `UidOnly` peers at compile time (`Config::pair_from_uid_only_peers`).
//!
//! The conversation requests (IPC v4) carry or reveal the user's content, so they are answered
//! ONLY for a `PeerCheck::SameSigner` peer: the development opt-in above never extends to them.
//! Typed text is validated here (empty, too long), then handed to the engine through the
//! bounded command queue (`queues::ENGINE_COMMANDS`: full => "busy" at once). Text is never
//! logged; counters only. The rest is read-only.
//!
//! Capture (spec S2 «punto único de consentimiento»): this module is the ONLY place that can
//! build a [`UserAct`], the one `capture::Start` of S2, and it does so only for a `CaptureStart`
//! from a same-signer connection that is subscribed to the conversation. The take belongs to
//! that connection: it keeps the sending half of an owner channel, so the take ends (`Discard`)
//! the moment the connection closes or stops being subscribed.

use crate::{
    agent::HealthProbe,
    capture::{OPEN_DEADLINE, Start},
    engine::Command,
    link::LinkView,
    pairing::Pairing,
    queues::IPC_MAX_CONNS,
    stats::Stats,
    time::deadline,
};
use fogar_ports::{
    clock::Clock,
    gateway_http::GatewayHttp,
    ipc::{
        CaptureGesture, CaptureRefusal, IPC_VERSION, IpcConn, IpcError, IpcRequest, IpcResponse,
        IpcServer, PeerCheck, SEND_TEXT_MAX_BYTES, StatusReport, TextRefusal,
    },
    tailnet::TailnetPeers,
};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

/// The person pressed the microphone button in the signed app, on a connection that shows the
/// conversation. The field is private to this module: nothing else in the crate (in particular
/// nothing on the gateway path) can make one, so no take can open without it.
#[derive(Debug)]
pub(crate) struct UserAct {
    gesture: CaptureGesture,
}

impl UserAct {
    pub(crate) fn gesture(&self) -> CaptureGesture {
        self.gesture
    }
    /// Unit tests of the gate only (compiled out of every real build).
    #[cfg(test)]
    pub(crate) fn for_test(gesture: CaptureGesture) -> Self {
        Self { gesture }
    }
}

/// Per-connection limits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// Each response write, including pushed `Event`s (a miss closes the connection).
    pub(crate) write_timeout: Duration,
    /// Waiting for the next complete request on an UNSUBSCRIBED connection; on expiry the
    /// connection is closed and its slot freed (review code#3/sec#5: `IPC_MAX_CONNS` silent
    /// clients would otherwise lock the service out forever).
    pub(crate) idle_timeout: Duration,
    /// IPC-4 development opt-in (compile-time at the composition root).
    pub(crate) pair_from_uid_only_peers: bool,
    /// How long `RequestMicPermission` waits for the person's answer to the system prompt.
    pub(crate) permission_wait: Duration,
}

pub(crate) async fn run<C: Clock, S: IpcServer, H: GatewayHttp, T: TailnetPeers>(
    server: S,
    clock: Arc<C>,
    health: HealthProbe,
    limits: Limits,
    pairing: Pairing<C, H, T>,
    cmds: mpsc::Sender<Command>,
    ingress: CancellationToken,
) {
    let mut conns = JoinSet::new();
    // Accept failures back off exponentially (250 ms .. 30 s) so a broken listener cannot spin.
    let mut fail_delay = Duration::from_millis(250);
    loop {
        tokio::select! {
            biased;
            () = ingress.cancelled() => break,
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            r = server.accept(), if conns.len() < IPC_MAX_CONNS => match r {
                Ok((peer, conn)) => {
                    // Per connection, as verified by the adapter (gates Pair/Unpair, IPC-4).
                    let peer_check = peer.check();
                    fail_delay = Duration::from_millis(250);
                    Stats::inc(&health.stats.ipc_connections);
                    conns.spawn(serve(Conn {
                        conn,
                        clock: clock.clone(),
                        health: health.clone(),
                        limits,
                        pairing: pairing.clone(),
                        cmds: cmds.clone(),
                        peer_check,
                        cancel: ingress.child_token(),
                    }));
                }
                Err(_) => {
                    tokio::select! {
                        biased;
                        () = ingress.cancelled() => break,
                        () = clock.sleep(fail_delay) => {}
                    }
                    fail_delay = (fail_delay * 2).min(Duration::from_secs(30));
                }
            },
        }
    }
    conns.shutdown().await;
}

struct Conn<K, C, H, T> {
    conn: K,
    clock: Arc<C>,
    health: HealthProbe,
    limits: Limits,
    pairing: Pairing<C, H, T>,
    cmds: mpsc::Sender<Command>,
    peer_check: PeerCheck,
    cancel: CancellationToken,
}

fn report(health: &HealthProbe, v: &LinkView, peer_check: PeerCheck) -> StatusReport {
    let s = health.stats.snapshot();
    let (transcriber, tts) = {
        let c = health.conversation.borrow();
        (c.transcriber, c.tts)
    };
    StatusReport::new(
        v.state.label().to_owned(),
        v.state.phase(),
        v.gateway.clone(),
        s.sessions_started,
        s.turns_completed,
        health.boot_mode.to_owned(),
        peer_check,
    )
    .with_voice(transcriber, tts)
}

impl<K: IpcConn, C: Clock, H: GatewayHttp, T: TailnetPeers> Conn<K, C, H, T> {
    /// `false` = the write failed or missed `write_timeout`: close the connection.
    async fn send(&mut self, resp: IpcResponse) -> bool {
        matches!(
            deadline(
                &*self.clock,
                self.limits.write_timeout,
                self.conn.send(resp)
            )
            .await,
            Some(Ok(()))
        )
    }

    fn may_pair(&self) -> bool {
        self.peer_check == PeerCheck::SameSigner || self.limits.pair_from_uid_only_peers
    }

    /// `&mut`: the future must stay `Send` without requiring `K: Sync`.
    async fn ask<R>(&mut self, make: impl FnOnce(oneshot::Sender<R>) -> Command) -> Option<R> {
        let timeout = self.limits.write_timeout;
        self.ask_within(timeout, make).await
    }

    async fn ask_within<R>(
        &mut self,
        timeout: Duration,
        make: impl FnOnce(oneshot::Sender<R>) -> Command,
    ) -> Option<R> {
        ask(&*self.clock, &self.cmds, timeout, &self.health.stats, make).await
    }
}

/// Hands a command to the engine and waits for its answer under `timeout`. `None` = busy: the
/// queue was full (never waited on) or the engine did not answer in time (the dropped reply
/// makes the engine skip the command).
async fn ask<C: Clock, R>(
    clock: &C,
    cmds: &mpsc::Sender<Command>,
    timeout: Duration,
    stats: &Stats,
    make: impl FnOnce(oneshot::Sender<R>) -> Command,
) -> Option<R> {
    let (tx, rx) = oneshot::channel();
    let answer = match cmds.try_send(make(tx)) {
        Ok(()) => deadline(clock, timeout, rx).await.and_then(Result::ok),
        Err(_) => None,
    };
    if answer.is_none() {
        Stats::inc(&stats.engine_commands_busy);
    }
    answer
}

/// The engine could not take the command now (full queue or no answer in time).
const BUSY: IpcResponse = IpcResponse::Refused {
    reason: "engine busy",
};

/// Local checks of a typed text before it reaches the engine (which adds `NotReady`).
fn check_text(text: &str) -> Result<(), TextRefusal> {
    if text.len() > SEND_TEXT_MAX_BYTES {
        Err(TextRefusal::TooLong)
    } else if text.trim().is_empty() {
        Err(TextRefusal::Empty)
    } else {
        Ok(())
    }
}

/// Runs a (bounded) operation unless shutdown starts first; `None` = shutdown.
async fn op<T>(cancel: &CancellationToken, f: impl Future<Output = T>) -> Option<T> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        v = f => Some(v),
    }
}

/// Next request; with `idle = None` (subscribed) it waits as long as the peer stays.
/// `None` = idle deadline passed.
async fn recv_within<C: Clock, K: IpcConn>(
    clock: &C,
    conn: &mut K,
    idle: Option<Duration>,
) -> Option<Result<Option<IpcRequest>, IpcError>> {
    match idle {
        Some(d) => deadline(clock, d, conn.recv()).await,
        None => Some(conn.recv().await),
    }
}

async fn serve<K: IpcConn, C: Clock, H: GatewayHttp, T: TailnetPeers>(mut c: Conn<K, C, H, T>) {
    let mut view = c.health.view.clone();
    let mut conv = c.health.conversation.clone();
    let mut subscribed = false;
    let mut conv_subscribed = false;
    // A peer that said another version is told ours, then refused (it must not guess).
    let mut version_ok = true;
    let mut last_pushed: Option<StatusReport> = None;
    // Sending half of the owner channel of the take this connection opened: dropping it (the
    // connection closes, or stops being subscribed) ends that take with `Discard`.
    let mut take_owner: Option<oneshot::Sender<()>> = None;
    loop {
        let req = tokio::select! {
            biased;
            () = c.cancel.cancelled() => return,
            // Requests BEFORE pushes (review M2): a dense token stream must never starve a
            // CancelTurn/SendText on a subscribed connection.
            r = recv_within(&*c.clock, &mut c.conn, (!subscribed && !conv_subscribed).then_some(c.limits.idle_timeout)) => match r {
                Some(Ok(Some(req))) => req,
                // Idle, peer gone or a bad frame: close the connection (dropping it).
                None | Some(Ok(None) | Err(_)) => return,
            },
            r = view.changed(), if subscribed => {
                if r.is_err() {
                    return;
                }
                let rep = report(&c.health, &view.borrow_and_update(), c.peer_check);
                if last_pushed.as_ref() == Some(&rep) {
                    continue;
                }
                if !c.send(IpcResponse::Event(rep.clone())).await {
                    return;
                }
                last_pushed = Some(rep);
                continue;
            }
            // The conversation, and the voice state inside the status report.
            r = conv.changed(), if subscribed || conv_subscribed => {
                if r.is_err() {
                    return;
                }
                let (latest, voice) = {
                    let v = conv.borrow_and_update();
                    // Status only: mark seen, no copy of the view.
                    (conv_subscribed.then(|| v.clone()), (v.transcriber, v.tts))
                };
                if let Some(latest) = latest
                    && !c.send(IpcResponse::Conversation(latest)).await
                {
                    return;
                }
                // Only a voice change rebuilds the status report (tokens alone never do).
                let pushed_voice = last_pushed.as_ref().map(|r| (r.transcriber, r.tts));
                if subscribed && pushed_voice != Some(voice) {
                    let rep = report(&c.health, &view.borrow(), c.peer_check);
                    if last_pushed.as_ref() != Some(&rep) {
                        if !c.send(IpcResponse::Event(rep.clone())).await {
                            return;
                        }
                        last_pushed = Some(rep);
                    }
                }
                continue;
            }
        };
        let resp = match req {
            IpcRequest::Hello { version } => {
                version_ok &= version == IPC_VERSION;
                // A peer speaking another version gets no more pushes either.
                subscribed &= version_ok;
                conv_subscribed &= version_ok;
                if !conv_subscribed {
                    take_owner = None;
                }
                IpcResponse::Hello {
                    version: IPC_VERSION,
                }
            }
            _ if !version_ok => IpcResponse::Refused {
                reason: "ipc version mismatch",
            },
            IpcRequest::Status => {
                IpcResponse::Status(report(&c.health, &view.borrow(), c.peer_check))
            }
            IpcRequest::Subscribe => {
                if !c.send(IpcResponse::Subscribed).await {
                    return;
                }
                let rep = report(&c.health, &view.borrow_and_update(), c.peer_check);
                subscribed = true;
                last_pushed = Some(rep.clone());
                IpcResponse::Event(rep)
            }
            IpcRequest::SubscribeConversation
            | IpcRequest::SendText { .. }
            | IpcRequest::CancelTurn
            | IpcRequest::CaptureStart { .. }
            | IpcRequest::CaptureEnd { .. }
            | IpcRequest::TurnVisible { .. }
            | IpcRequest::RequestMicPermission
                if c.peer_check != PeerCheck::SameSigner =>
            {
                IpcResponse::Refused {
                    reason: "needs the signed Fogar app",
                }
            }
            IpcRequest::SubscribeConversation => {
                if !c.send(IpcResponse::ConversationSubscribed).await {
                    return;
                }
                conv_subscribed = true;
                IpcResponse::Conversation(conv.borrow_and_update().clone())
            }
            // `TextAccepted` / `TurnCancelSent` mean "accepted by the agent", NOT "delivered to
            // the gateway": the conversation view carries the real outcome (an uplink failure
            // ends the turn there as `Error` / `Interrupted`).
            IpcRequest::SendText { text } => match check_text(&text.0) {
                Err(r) => IpcResponse::TextRefused(r),
                Ok(()) => match c.ask(|tx| Command::SendText(text.0, tx)).await {
                    Some(Ok(())) => IpcResponse::TextAccepted,
                    Some(Err(r)) => IpcResponse::TextRefused(r),
                    None => BUSY,
                },
            },
            IpcRequest::CancelTurn => match c.ask(Command::Cancel).await {
                Some(()) => IpcResponse::TurnCancelSent,
                None => BUSY,
            },
            IpcRequest::Discover => match op(&c.cancel, c.pairing.discover(c.may_pair())).await {
                Some(found) => IpcResponse::Discovered(found),
                None => return,
            },
            IpcRequest::Pair { .. } | IpcRequest::Unpair | IpcRequest::Reconnect
                if !c.may_pair() =>
            {
                IpcResponse::Refused {
                    reason: "needs the signed Fogar app",
                }
            }
            IpcRequest::Pair { address, code } => {
                match op(&c.cancel, c.pairing.pair(&address, code.as_ref())).await {
                    Some(Ok(info)) => IpcResponse::Paired(info),
                    Some(Err(f)) => IpcResponse::PairFailed(f),
                    None => return,
                }
            }
            IpcRequest::Unpair => match op(&c.cancel, c.pairing.unpair()).await {
                Some(Ok(())) => IpcResponse::Unpaired,
                Some(Err(_)) => IpcResponse::Refused {
                    reason: "could not delete the pairing",
                },
                None => return,
            },
            // Checked against the published state: the link only listens while parked for such a
            // reason, and discards a request it saw before parking.
            IpcRequest::Reconnect if view.borrow().state.user_may_reconnect() => {
                c.pairing.reconnect.send_replace(());
                IpcResponse::Reconnecting
            }
            IpcRequest::Reconnect => IpcResponse::Refused {
                reason: "nothing to reconnect",
            },
            IpcRequest::CaptureStart { .. } if !conv_subscribed => {
                Stats::inc(&c.health.stats.captures_refused_unsubscribed);
                IpcResponse::CaptureRefused(CaptureRefusal::NotSubscribed)
            }
            // This connection's own take is still open: never drop its owner (that would end it).
            IpcRequest::CaptureStart { .. }
                if take_owner.as_ref().is_some_and(|o| !o.is_closed()) =>
            {
                Stats::inc(&c.health.stats.captures_refused_busy);
                IpcResponse::CaptureRefused(CaptureRefusal::Busy)
            }
            IpcRequest::CaptureStart { gesture } => {
                let (owner_tx, owner) = oneshot::channel();
                take_owner = Some(owner_tx);
                let start = Start::UserAct(UserAct { gesture });
                // The engine answers after the device opened (off the runtime, bounded).
                let within = c.limits.write_timeout + OPEN_DEADLINE;
                match c
                    .ask_within(within, |reply| Command::CaptureStart {
                        start,
                        owner,
                        reply,
                    })
                    .await
                {
                    Some(Ok(())) => IpcResponse::CaptureStarted,
                    Some(Err(r)) => IpcResponse::CaptureRefused(r),
                    None => BUSY,
                }
            }
            IpcRequest::CaptureEnd { then } => {
                match c.ask(|reply| Command::CaptureEnd(then, reply)).await {
                    Some(()) => IpcResponse::CaptureEnded,
                    None => BUSY,
                }
            }
            IpcRequest::RequestMicPermission => {
                let within = c.limits.permission_wait;
                match c.ask_within(within, Command::RequestMicPermission).await {
                    Some(p) => IpcResponse::MicPermission(p),
                    // Not answered in time: the status as it stands (never a guess).
                    None => IpcResponse::MicPermission(conv.borrow().mic),
                }
            }
            // TurnVisible arrives with S2 3c (the playback gate).
            _ => IpcResponse::Refused {
                reason: "unsupported request",
            },
        };
        if !c.send(resp).await {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::disallowed_methods)]
    use super::*;
    use crate::queues::ENGINE_COMMANDS;
    use fogar_testkit::TestClock;

    /// `queues`: a full engine queue is answered "busy" at once, without waiting.
    #[tokio::test(start_paused = true)]
    async fn full_command_queue_is_busy_at_once() {
        let (tx, mut rx) = mpsc::channel(ENGINE_COMMANDS);
        let (clock, stats) = (TestClock::new(), Stats::default());
        let mut parked = Vec::new();
        for _ in 0..ENGINE_COMMANDS {
            let (r, keep) = oneshot::channel::<()>();
            tx.try_send(Command::Cancel(r)).unwrap();
            parked.push(keep);
        }
        let t0 = tokio::time::Instant::now();
        let got = ask(&clock, &tx, Duration::from_secs(5), &stats, Command::Cancel).await;
        assert_eq!((got, t0.elapsed()), (None, Duration::ZERO));
        assert_eq!(stats.snapshot().engine_commands_busy, 1);
        // Room again, but nobody answers: busy after the deadline, and the reply is abandoned
        // so the engine skips the command instead of running it late.
        drop(rx.recv().await);
        let got = ask(&clock, &tx, Duration::from_secs(5), &stats, Command::Cancel).await;
        assert_eq!(got, None);
        assert_eq!(t0.elapsed(), Duration::from_secs(5));
        let mut last = None;
        while let Ok(c) = rx.try_recv() {
            last = Some(c);
        }
        assert!(matches!(last, Some(Command::Cancel(r)) if r.is_closed()));
    }

    #[test]
    fn typed_text_checks() {
        let max = "a".repeat(SEND_TEXT_MAX_BYTES);
        for (text, want) in [
            ("hola", Ok(())),
            ("", Err(TextRefusal::Empty)),
            (" \n\t ", Err(TextRefusal::Empty)),
            (max.as_str(), Ok(())),
        ] {
            assert_eq!(check_text(text), want, "{text:?}");
        }
        let over = format!("{max}a");
        assert_eq!(check_text(&over), Err(TextRefusal::TooLong));
    }
}
