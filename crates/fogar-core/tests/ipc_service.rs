//! Local UI service limits (review code#3 / sec#5): silent connections cannot hold the
//! `IPC_MAX_CONNS` slots forever.
#![allow(clippy::disallowed_methods)] // the harness drives tokio's paused clock directly

mod common;
use common::*;
use fogar_core::queues::IPC_MAX_CONNS;
use fogar_ports::ipc::{IpcRequest, IpcResponse};
use fogar_testkit::MockConfig;
use std::time::Duration;

#[tokio::test(start_paused = true)]
async fn silent_clients_cannot_lock_out_the_service() {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    let idle = b.cfg.ipc_idle_timeout;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let ipc = rig.ipc.clone().unwrap();
    let mut silent = Vec::new();
    for _ in 0..IPC_MAX_CONNS {
        silent.push(ipc.client().await);
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut c = ipc.client().await;
    let r = tokio::time::timeout(idle + Duration::from_secs(5), c.request(IpcRequest::Status))
        .await
        .expect("5th client starved by silent connections");
    assert!(matches!(r, Some(IpcResponse::Status(_))), "{r:?}");
    drop(silent);
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}

#[tokio::test(start_paused = true)]
async fn an_active_client_is_not_cut_off() {
    let mut b = RigBuilder::fake(MockConfig::default());
    b.with_ipc = true;
    let idle = b.cfg.ipc_idle_timeout;
    let mut rig = b.start();
    wait_ready(&mut rig.health).await;
    let ipc = rig.ipc.clone().unwrap();
    let mut c = ipc.client().await;
    for _ in 0..10 {
        tokio::time::sleep(idle / 2).await;
        assert!(matches!(
            c.request(IpcRequest::Status).await,
            Some(IpcResponse::Status(_))
        ));
    }
    assert!(rig.agent.shutdown(Duration::from_secs(4)).await.clean);
}
