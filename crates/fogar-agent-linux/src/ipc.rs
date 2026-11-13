//! Unix-domain-socket IPC (ADR-0002) with peer verification by `SO_PEERCRED` uid
//! (fogar-secure-rust IPC-1..IPC-5). Wire format: 4-byte big-endian length + JSON, max
//! `MAX_FRAME_BYTES`, typed messages with `deny_unknown_fields`.
//!
//! macOS adds the audit-token -> SecCode designated-requirement check in fogar-agent-macos; this
//! Linux adapter only has the uid check (`peer_code_identity: Unsupported`).

use fogar_ports::{
    Support,
    ipc::{
        IpcCaps, IpcConn, IpcError, IpcRequest, IpcResponse, IpcServer, MAX_FRAME_BYTES,
        PeerIdentity,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};

/// `sun_path` is 108 bytes on Linux and 104 on macOS: keep the stricter limit everywhere.
const MAX_SOCKET_PATH: usize = 103;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum WireRequest {
    Hello { version: u32 },
    Status {},
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum WireResponse {
    Hello {
        version: u32,
    },
    Status {
        link: String,
        sessions: u64,
        turns_completed: u64,
        boot_mode: String,
    },
    Refused {
        reason: String,
    },
}

#[derive(Debug)]
pub struct UdsIpcServer {
    listener: UnixListener,
    path: PathBuf,
    uid: u32,
}

fn io_err(e: std::io::Error) -> IpcError {
    IpcError::Transport(Box::new(e))
}

impl UdsIpcServer {
    /// Binds `<dir>/agent.sock`. `dir` is created 0700 and must be ours. A stale socket is removed
    /// only if nothing answers on it and it is a socket we own (never follows symlinks).
    pub fn bind(dir: &Path) -> std::io::Result<Self> {
        crate::ensure_private_dir(dir)?;
        let path = dir.join("agent.sock");
        if path.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "socket path too long",
            ));
        }
        let uid = std::fs::metadata("/proc/self")?.uid();
        if let Ok(md) = std::fs::symlink_metadata(&path) {
            if !md.file_type().is_socket() || md.uid() != uid {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "socket path is occupied",
                ));
            }
            if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "another agent is running",
                ));
            }
            std::fs::remove_file(&path)?;
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            path,
            uid,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for UdsIpcServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl IpcServer for UdsIpcServer {
    type Conn = UdsConn;

    fn capabilities(&self) -> IpcCaps {
        let mut c = IpcCaps::NONE;
        c.peer_uid = Support::Native;
        c
    }

    async fn accept(&self) -> Result<(PeerIdentity, UdsConn), IpcError> {
        loop {
            // Cancel-safe: `UnixListener::accept` is.
            let (stream, _) = self.listener.accept().await.map_err(io_err)?;
            match stream.peer_cred() {
                Ok(c) if c.uid() == self.uid => {
                    return Ok((PeerIdentity::new(Some(c.uid()), None), UdsConn::new(stream)));
                }
                // Another user (or unknown): drop the connection before reading a byte.
                _ => continue,
            }
        }
    }
}

/// Framed connection. `recv` keeps partial frames in `buf` so it is cancel-safe.
#[derive(Debug)]
pub struct UdsConn {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl UdsConn {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            buf: Vec::with_capacity(512),
        }
    }
}

/// Pops one complete frame from `buf` if present.
fn take_frame(buf: &mut Vec<u8>) -> Result<Option<Vec<u8>>, IpcError> {
    let Some(hdr) = buf.first_chunk::<4>() else {
        return Ok(None);
    };
    let len = usize::try_from(u32::from_be_bytes(*hdr)).unwrap_or(usize::MAX);
    if len > MAX_FRAME_BYTES {
        return Err(IpcError::TooLarge {
            len,
            max: MAX_FRAME_BYTES,
        });
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let body = buf[4..4 + len].to_vec();
    buf.drain(..4 + len);
    Ok(Some(body))
}

async fn read_frame(
    stream: &mut UnixStream,
    buf: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>, IpcError> {
    loop {
        if let Some(f) = take_frame(buf)? {
            return Ok(Some(f));
        }
        if buf.len() >= 4 + MAX_FRAME_BYTES {
            return Err(IpcError::TooLarge {
                len: buf.len(),
                max: MAX_FRAME_BYTES,
            });
        }
        // Cancel-safe: `read_buf` either appended bytes (kept in `buf`) or did nothing.
        let n = stream.read_buf(buf).await.map_err(io_err)?;
        if n == 0 {
            return Ok(None);
        }
    }
}

async fn write_frame(stream: &mut UnixStream, body: &[u8]) -> Result<(), IpcError> {
    if body.len() > MAX_FRAME_BYTES {
        return Err(IpcError::TooLarge {
            len: body.len(),
            max: MAX_FRAME_BYTES,
        });
    }
    let len = u32::try_from(body.len()).map_err(|_| IpcError::Malformed)?;
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    stream.write_all(&out).await.map_err(io_err)
}

impl IpcConn for UdsConn {
    async fn recv(&mut self) -> Result<Option<IpcRequest>, IpcError> {
        let Some(body) = read_frame(&mut self.stream, &mut self.buf).await? else {
            return Ok(None);
        };
        match serde_json::from_slice::<WireRequest>(&body).map_err(|_| IpcError::Malformed)? {
            WireRequest::Hello { version } => Ok(Some(IpcRequest::Hello { version })),
            WireRequest::Status {} => Ok(Some(IpcRequest::Status)),
        }
    }

    async fn send(&mut self, resp: IpcResponse) -> Result<(), IpcError> {
        let w = match resp {
            IpcResponse::Hello { version } => WireResponse::Hello { version },
            IpcResponse::Status(s) => WireResponse::Status {
                link: s.link,
                sessions: s.sessions,
                turns_completed: s.turns_completed,
                boot_mode: s.boot_mode,
            },
            IpcResponse::Refused { reason } => WireResponse::Refused {
                reason: reason.to_owned(),
            },
            _ => WireResponse::Refused {
                reason: "unsupported response".to_owned(),
            },
        };
        let body = serde_json::to_vec(&w).map_err(|_| IpcError::Malformed)?;
        write_frame(&mut self.stream, &body).await
    }
}

/// Client side of the wire format (CLI/status tool, contract tests, the Swift app's reference).
#[derive(Debug)]
pub struct UdsIpcClient {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl UdsIpcClient {
    pub async fn connect(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            stream: UnixStream::connect(path).await?,
            buf: Vec::new(),
        })
    }

    pub async fn request(&mut self, r: IpcRequest) -> Option<IpcResponse> {
        let w = match r {
            IpcRequest::Hello { version } => WireRequest::Hello { version },
            IpcRequest::Status => WireRequest::Status {},
            _ => return None,
        };
        let body = serde_json::to_vec(&w).ok()?;
        write_frame(&mut self.stream, &body).await.ok()?;
        let resp = read_frame(&mut self.stream, &mut self.buf).await.ok()??;
        Some(match serde_json::from_slice::<WireResponse>(&resp).ok()? {
            WireResponse::Hello { version } => IpcResponse::Hello { version },
            WireResponse::Status {
                link,
                sessions,
                turns_completed,
                boot_mode,
            } => IpcResponse::Status(fogar_ports::ipc::StatusReport::new(
                link,
                sessions,
                turns_completed,
                boot_mode,
            )),
            WireResponse::Refused { .. } => IpcResponse::Refused { reason: "refused" },
        })
    }

    /// Sends raw bytes (adversarial tests: oversize frames, garbage).
    pub async fn send_raw(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.stream.write_all(bytes).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_limits() {
        let mut b = vec![0, 0, 0, 2, b'{', b'}', 9];
        assert_eq!(take_frame(&mut b).unwrap(), Some(b"{}".to_vec()));
        assert_eq!(b, vec![9]);
        let mut big = (u32::try_from(MAX_FRAME_BYTES).unwrap() + 1)
            .to_be_bytes()
            .to_vec();
        assert!(matches!(
            take_frame(&mut big),
            Err(IpcError::TooLarge { .. })
        ));
        assert!(
            serde_json::from_slice::<WireRequest>(br#"{"type":"status","exec":"rm -rf"}"#).is_err()
        );
        assert!(serde_json::from_slice::<WireRequest>(br#"{"type":"run","cmd":"x"}"#).is_err());
    }
}
