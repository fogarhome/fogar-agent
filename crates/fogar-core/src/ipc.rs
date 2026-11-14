//! Local UI service over the IpcServer port (ADR-0002). Read-only in slice 0: version handshake
//! and status. Connections live in a JoinSet drained in the same loop and aborted on shutdown.

use crate::{agent::HealthProbe, queues::IPC_MAX_CONNS, stats::Stats, time::deadline};
use fogar_ports::{
    clock::Clock,
    ipc::{IPC_VERSION, IpcConn, IpcRequest, IpcResponse, IpcServer, StatusReport},
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
                Ok((_peer, conn)) => {
                    fail_delay = Duration::from_millis(250);
                    Stats::inc(&health.stats.ipc_connections);
                    conns.spawn(serve(conn, clock.clone(), health.clone(), limits, ingress.child_token()));
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
            Ok(Some(IpcRequest::Hello { .. })) => IpcResponse::Refused {
                reason: "unsupported ipc version",
            },
            Ok(Some(IpcRequest::Status)) => {
                let h = health.snapshot();
                IpcResponse::Status(StatusReport::new(
                    h.link.label().to_owned(),
                    h.stats.sessions_started,
                    h.stats.turns_completed,
                    h.boot_mode.to_owned(),
                ))
            }
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
