//! Local UI service over the IpcServer port (ADR-0002). Read-only in slice 0: version handshake
//! and status. Connections live in a JoinSet drained in the same loop and aborted on shutdown.

use crate::{agent::HealthProbe, queues::IPC_MAX_CONNS, stats::Stats, time::deadline};
use fogar_ports::{
    clock::Clock,
    ipc::{IPC_VERSION, IpcConn, IpcRequest, IpcResponse, IpcServer, PeerCheck, StatusReport},
};
use std::{sync::Arc, time::Duration};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Per-connection deadlines.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// Each response write.
    pub(crate) write_timeout: Duration,
    /// Waiting for the next complete request; on expiry the connection is closed and its slot
    /// freed (review code#3/sec#5: `IPC_MAX_CONNS` silent clients would otherwise lock the
    /// service out forever).
    pub(crate) idle_timeout: Duration,
}

pub(crate) async fn run<C: Clock, S: IpcServer>(
    server: S,
    clock: Arc<C>,
    health: HealthProbe,
    limits: Limits,
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
                    conns.spawn(serve(conn, clock.clone(), health.clone(), limits, peer_check, ingress.child_token()));
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

async fn serve<C: Clock, K: IpcConn>(
    mut conn: K,
    clock: Arc<C>,
    health: HealthProbe,
    limits: Limits,
    peer_check: PeerCheck,
    cancel: CancellationToken,
) {
    loop {
        let req = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            r = deadline(&*clock, limits.idle_timeout, conn.recv()) => match r {
                Some(r) => r,
                // Idle: close the connection (dropping it) and free the slot.
                None => return,
            },
        };
        let resp = match req {
            Ok(Some(IpcRequest::Hello { version })) if version == IPC_VERSION => {
                IpcResponse::Hello {
                    version: IPC_VERSION,
                }
            }
            // Version mismatch: answer with ours so the client can tell the person which side
            // to update (it compares and stops).
            Ok(Some(IpcRequest::Hello { .. })) => IpcResponse::Hello {
                version: IPC_VERSION,
            },
            Ok(Some(IpcRequest::Status)) => {
                let h = health.snapshot();
                IpcResponse::Status(StatusReport::new(
                    h.link.label().to_owned(),
                    h.link.phase(),
                    // TODO(S1 core PR): gateway info from the pairing record.
                    None,
                    h.stats.sessions_started,
                    h.stats.turns_completed,
                    h.boot_mode.to_owned(),
                    peer_check,
                ))
            }
            // TODO(S1 core PR): subscribe (watch, exempt from idle_timeout, push deadline closes),
            // discover, pair, unpair (Pair/Unpair only for PeerCheck::SameSigner unless the
            // composition root opts in). Listed explicitly so a new variant is a conscious
            // decision.
            Ok(Some(
                IpcRequest::Subscribe
                | IpcRequest::Discover
                | IpcRequest::Pair { .. }
                | IpcRequest::Unpair,
            )) => IpcResponse::Refused {
                reason: "not implemented yet",
            },
            Ok(Some(_)) => IpcResponse::Refused {
                reason: "unsupported request",
            },
            Ok(None) | Err(_) => return,
        };
        if !matches!(
            deadline(&*clock, limits.write_timeout, conn.send(resp)).await,
            Some(Ok(()))
        ) {
            return;
        }
    }
}
