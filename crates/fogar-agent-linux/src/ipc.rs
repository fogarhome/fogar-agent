//! Unix-domain-socket IPC (ADR-0002) with peer verification by `SO_PEERCRED` uid
//! (fogar-secure-rust IPC-1..IPC-5). Wire format and framing: `fogar_host::ipc_wire` (shared
//! with the macOS adapter and the Swift app).
//!
//! macOS adds the audit-token -> SecCode designated-requirement check in fogar-agent-macos; this
//! Linux adapter only has the uid check (`peer_code_identity: Unsupported`).

use fogar_host::ipc_wire::{
    decode_request, decode_response, encode_request, encode_response, frame, take_frame,
};
use fogar_ports::{
    Support,
    ipc::{
        IpcCaps, IpcConn, IpcError, IpcRequest, IpcResponse, IpcServer, MAX_FRAME_BYTES,
        PeerIdentity,
    },
};
use std::{
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};
use zeroize::Zeroizing;

/// `sun_path` is 108 bytes on Linux and 104 on macOS: keep the stricter limit everywhere.
const MAX_SOCKET_PATH: usize = 103;

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
                    return Ok((PeerIdentity::uid_only(Some(c.uid())), UdsConn::new(stream)));
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

async fn read_frame(
    stream: &mut UnixStream,
    buf: &mut Vec<u8>,
) -> Result<Option<Zeroizing<Vec<u8>>>, IpcError> {
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
    stream.write_all(&frame(body)?).await.map_err(io_err)
}

impl IpcConn for UdsConn {
    async fn recv(&mut self) -> Result<Option<IpcRequest>, IpcError> {
        let Some(body) = read_frame(&mut self.stream, &mut self.buf).await? else {
            return Ok(None);
        };
        decode_request(&body).map(Some)
    }

    async fn send(&mut self, resp: IpcResponse) -> Result<(), IpcError> {
        write_frame(&mut self.stream, &encode_response(resp)?).await
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
        let body = encode_request(&r).ok()?;
        write_frame(&mut self.stream, &body).await.ok()?;
        self.next().await
    }

    /// Next message without sending (pushed events after `Subscribe`). `None` = closed.
    pub async fn next(&mut self) -> Option<IpcResponse> {
        let resp = read_frame(&mut self.stream, &mut self.buf).await.ok()??;
        decode_response(&resp).ok()
    }

    /// Writes the first (`first = true`) or second half of `r`'s frame (cancel-safety clause).
    pub async fn send_half(&mut self, r: &IpcRequest, first: bool) -> Option<()> {
        let f = frame(&encode_request(r).ok()?).ok()?;
        let mid = f.len() / 2;
        let part = if first { &f[..mid] } else { &f[mid..] };
        self.stream.write_all(part).await.ok()
    }

    /// Sends raw bytes (adversarial tests: oversize frames, garbage).
    pub async fn send_raw(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.stream.write_all(bytes).await
    }
}
