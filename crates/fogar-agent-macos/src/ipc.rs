//! UNTESTED STUB: no listener in slice 0; `accept` never resolves (no polling, no wakeups).
//!
//! TODO(S4, ADR-0002, fogar-macos-daemon §IPC): Unix domain socket at
//! `~/Library/Application Support/com.fogar.agent/run/agent.sock` (dir 0700, socket 0600, path
//! < 104 bytes, single instance via `flock`, unlink a stale socket only after `connect` fails);
//! per connection: `getsockopt(SOL_LOCAL, LOCAL_PEERTOKEN)` -> audit token ->
//! `SecCodeCopyGuestWithAttributes` (`kSecGuestAttributeAudit`) -> `SecCodeCheckValidity` against
//! the Team-ID designated requirement of the Swift app; uid via `getpeereid` as a first gate.
//! Never authorize by PID (`LOCAL_PEERPID`). Same framing as fogar-agent-linux `UdsConn`.

use fogar_ports::ipc::{IpcCaps, IpcConn, IpcError, IpcRequest, IpcResponse, IpcServer, PeerIdentity};

#[derive(Debug, Clone, Copy, Default)]
pub struct MacIpcServer;

/// Uninhabited until S4.
#[derive(Debug)]
pub enum MacIpcConn {}

impl IpcConn for MacIpcConn {
    async fn recv(&mut self) -> Result<Option<IpcRequest>, IpcError> {
        match *self {}
    }
    async fn send(&mut self, _resp: IpcResponse) -> Result<(), IpcError> {
        match *self {}
    }
}

impl IpcServer for MacIpcServer {
    type Conn = MacIpcConn;
    fn capabilities(&self) -> IpcCaps {
        IpcCaps::NONE
    }
    async fn accept(&self) -> Result<(PeerIdentity, MacIpcConn), IpcError> {
        std::future::pending().await
    }
}
