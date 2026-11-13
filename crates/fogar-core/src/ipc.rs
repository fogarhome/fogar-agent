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

pub(crate) async fn run<C: Clock, S: IpcServer>(
    server: S,
    clock: Arc<C>,
    health: HealthProbe,
    write_timeout: Duration,
    ingress: CancellationToken,
) {
    let mut conns = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            () = ingress.cancelled() => break,
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            r = server.accept(), if conns.len() < IPC_MAX_CONNS => match r {
                Ok((_peer, conn)) => {
                    Stats::inc(&health.stats.ipc_connections);
                    conns.spawn(serve(conn, clock.clone(), health.clone(), write_timeout, ingress.child_token()));
                }
                // Transient accept failure: brief pause so a broken listener cannot hot-loop.
                Err(_) => clock.sleep(Duration::from_millis(250)).await,
            },
        }
    }
    conns.shutdown().await;
}

async fn serve<C: Clock, K: IpcConn>(
    mut conn: K,
    clock: Arc<C>,
    health: HealthProbe,
    write_timeout: Duration,
    cancel: CancellationToken,
) {
    loop {
        let req = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            r = conn.recv() => r,
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
            deadline(&*clock, write_timeout, conn.send(resp)).await,
            Some(Ok(()))
        ) {
            return;
        }
    }
}
