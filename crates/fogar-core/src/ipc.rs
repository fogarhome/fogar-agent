//! Local UI service over the IpcServer port (ADR-0002): version handshake, status, status
//! subscription, discovery and pairing. Connections live in a JoinSet drained in the same loop
//! and aborted on shutdown.
//!
//! Authorization (IPC-4): `Pair`/`Unpair` change where the user's voice goes and `Reconnect`
//! makes the agent spend the stored key to take the conversation back from another session (any
//! same-user process could otherwise drive the 4102 ping-pong spec §13 forbids), so they are
//! answered only for a `PeerCheck::SameSigner` peer, unless the composition root opted in to
//! `UidOnly` peers at compile time (`Config::pair_from_uid_only_peers`). The rest is read-only.

use crate::{
    agent::HealthProbe, link::LinkView, pairing::Pairing, queues::IPC_MAX_CONNS, stats::Stats,
    time::deadline,
};
use fogar_ports::{
    clock::Clock,
    gateway_http::GatewayHttp,
    ipc::{
        IPC_VERSION, IpcConn, IpcError, IpcRequest, IpcResponse, IpcServer, PeerCheck, StatusReport,
    },
};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

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
}

pub(crate) async fn run<C: Clock, S: IpcServer, H: GatewayHttp>(
    server: S,
    clock: Arc<C>,
    health: HealthProbe,
    limits: Limits,
    pairing: Pairing<C, H>,
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

struct Conn<K, C, H> {
    conn: K,
    clock: Arc<C>,
    health: HealthProbe,
    limits: Limits,
    pairing: Pairing<C, H>,
    peer_check: PeerCheck,
    cancel: CancellationToken,
}

fn report(health: &HealthProbe, v: &LinkView, peer_check: PeerCheck) -> StatusReport {
    let s = health.stats.snapshot();
    StatusReport::new(
        v.state.label().to_owned(),
        v.state.phase(),
        v.gateway.clone(),
        s.sessions_started,
        s.turns_completed,
        health.boot_mode.to_owned(),
        peer_check,
    )
}

impl<K: IpcConn, C: Clock, H: GatewayHttp> Conn<K, C, H> {
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

async fn serve<K: IpcConn, C: Clock, H: GatewayHttp>(mut c: Conn<K, C, H>) {
    let mut view = c.health.view.clone();
    let mut subscribed = false;
    // A peer that said another version is told ours, then refused (it must not guess).
    let mut version_ok = true;
    let mut last_pushed: Option<StatusReport> = None;
    loop {
        let req = tokio::select! {
            biased;
            () = c.cancel.cancelled() => return,
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
            r = recv_within(&*c.clock, &mut c.conn, (!subscribed).then_some(c.limits.idle_timeout)) => match r {
                Some(Ok(Some(req))) => req,
                // Idle, peer gone or a bad frame: close the connection (dropping it).
                None | Some(Ok(None) | Err(_)) => return,
            },
        };
        let resp = match req {
            IpcRequest::Hello { version } => {
                version_ok &= version == IPC_VERSION;
                // A peer speaking another version gets no more pushes either.
                subscribed &= version_ok;
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
            IpcRequest::Discover => match op(&c.cancel, c.pairing.discover()).await {
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
            _ => IpcResponse::Refused {
                reason: "unsupported request",
            },
        };
        if !c.send(resp).await {
            return;
        }
    }
}
