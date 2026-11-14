//! Unix-domain-socket IPC (ADR-0002 + amendment 2026-11-14) with a two-layer peer check
//! (fogar-secure-rust IPC-1..IPC-5). Wire format and framing: `fogar_host::ipc_wire`, shared with
//! the Linux adapter and the Swift app.
//!
//! Per connection, before a byte is read:
//! 1. uid gate: `getpeereid` (tokio `peer_cred`) must equal our euid, else the socket is dropped.
//! 2. code identity: `getsockopt(SOL_LOCAL, LOCAL_PEERTOKEN)` → audit token →
//!    `SecCodeCopyGuestWithAttributes(kSecGuestAttributeAudit)` → `SecCodeCheckValidity` against
//!    `anchor apple generic and certificate leaf[subject.OU] = "<our Team ID>" and identifier
//!    "<expected id>"`. Pass → [`PeerIdentity::verified_signer`]; any failure → `uid_only`.
//!
//! Our Team ID is read from the agent's OWN signature at bind time, so the same code works with a
//! free Personal Team now and Developer ID later. An unsigned/ad-hoc agent has no Team ID: it
//! skips step 2 and reports `peer_code_identity: Unsupported`. Never authorized by PID.
//! Known limit (skill ipc.md §1): identity is checked at connect time; a verified peer could hand
//! its connected fd to another same-user process.

use core_foundation::{
    base::{CFType, TCFType},
    data::CFData,
    dictionary::{CFDictionary, CFDictionaryRef},
    string::{CFString, CFStringRef},
};
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
use security_framework::os::macos::code_signing::{Flags, GuestAttributes, SecCode};
use std::{
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};
use zeroize::Zeroizing;

/// `sun_path` is 104 bytes on macOS (incl. NUL).
const MAX_SOCKET_PATH: usize = 103;

/// Bundle identifier of the companion app (`apps/macos/`). GUESSED (S1 spec: "Bundle id
/// `com.fogar.desktop` (GUESSED)"); frozen once the app ships.
pub const COMPANION_APP_ID: &str = "com.fogar.desktop";

/// `kSecCSSigningInformation` (`<Security/SecCode.h>`).
const K_SEC_CS_SIGNING_INFORMATION: u32 = 1 << 1;

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    /// `<Security/SecCode.h>`; accepts a `SecCodeRef` where a `SecStaticCodeRef` is expected.
    fn SecCodeCopySigningInformation(
        code: *const std::ffi::c_void,
        flags: u32,
        information: *mut CFDictionaryRef,
    ) -> i32;
    static kSecCodeInfoTeamIdentifier: CFStringRef;
}

fn io_err(e: std::io::Error) -> IpcError {
    IpcError::Transport(Box::new(e))
}

fn euid() -> u32 {
    // SAFETY: geteuid takes no arguments, cannot fail and touches no memory we own.
    unsafe { libc::geteuid() }
}

/// Creates (or validates) a private directory: owned by us, mode 0700, not a symlink.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let md = std::fs::symlink_metadata(dir)?;
    if md.file_type().is_symlink() || !md.is_dir() || md.uid() != euid() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "not a private directory we own",
        ));
    }
    if md.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Team IDs are 10 uppercase alphanumerics; anything else is never interpolated into a
/// requirement (no injection into the requirement language).
fn is_team_id(s: &str) -> bool {
    s.len() == 10
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// Reverse-DNS bundle identifier characters only.
fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

/// The Team ID of the running agent, if it is signed with a certificate (not ad-hoc) and its
/// signature is currently valid for that team.
fn own_team_id() -> Option<String> {
    let me = SecCode::for_self(Flags::NONE).ok()?;
    let mut info: CFDictionaryRef = std::ptr::null();
    // SAFETY: `me` is a live SecCodeRef for the duration of the call (Apple documents SecCodeRef
    // as accepted here); `info` is a valid out-pointer; on success we own the returned dictionary
    // (Copy rule) and wrap it exactly once below.
    let rc = unsafe {
        SecCodeCopySigningInformation(
            me.as_concrete_TypeRef().cast(),
            K_SEC_CS_SIGNING_INFORMATION,
            &mut info,
        )
    };
    if rc != 0 || info.is_null() {
        return None;
    }
    // SAFETY: non-null CFDictionaryRef returned by a Copy function: create rule, released on drop.
    let dict: CFDictionary<CFString, CFType> =
        unsafe { CFDictionary::wrap_under_create_rule(info) };
    // SAFETY: an immutable CFString constant exported by Security.framework; get rule retains it.
    let key = unsafe { CFString::wrap_under_get_rule(kSecCodeInfoTeamIdentifier) };
    let team = dict.find(&key)?.downcast::<CFString>()?.to_string();
    if !is_team_id(&team) {
        return None;
    }
    // Double check: the agent itself satisfies the team requirement it will demand of peers.
    let req = format!(r#"anchor apple generic and certificate leaf[subject.OU] = "{team}""#)
        .parse()
        .ok()?;
    me.check_validity(Flags::NONE, &req).ok()?;
    Some(team)
}

/// The peer's audit token (`audit_token_t`, 8 × u32), or `None`.
fn peer_audit_token(stream: &UnixStream) -> Option<[u32; 8]> {
    let mut tok = [0u32; 8];
    let mut len = std::mem::size_of_val(&tok) as libc::socklen_t;
    // SAFETY: the fd is a live connected AF_UNIX socket owned by `stream`; `tok` is a writable,
    // aligned 32-byte buffer and `len` holds its exact size; the kernel writes at most `len` bytes
    // and updates `len`.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERTOKEN,
            tok.as_mut_ptr().cast(),
            &mut len,
        )
    };
    (rc == 0 && len as usize == std::mem::size_of_val(&tok)).then_some(tok)
}

/// Whether the process behind `token` satisfies `requirement` right now.
fn token_satisfies(token: &[u32; 8], requirement: &str) -> bool {
    let bytes: Vec<u8> = token.iter().flat_map(|v| v.to_ne_bytes()).collect();
    let data = CFData::from_buffer(&bytes);
    let mut attrs = GuestAttributes::new();
    // `data` outlives `attrs` (the dictionary retains it anyway).
    attrs.set_audit_token(data.as_concrete_TypeRef());
    let Ok(code) = SecCode::copy_guest_with_attribues(None, &attrs, Flags::NONE) else {
        return false;
    };
    let Ok(req) = requirement.parse() else {
        return false;
    };
    code.check_validity(Flags::NONE, &req).is_ok()
}

#[derive(Debug)]
pub struct MacIpcServer {
    listener: UnixListener,
    path: PathBuf,
    uid: u32,
    /// `Some` only when the agent is signed with a certificate: (requirement, expected id).
    signer: Option<(String, String)>,
}

impl MacIpcServer {
    /// `~/Library/Application Support/com.fogar.agent/run` (ADR-0002); `None` without an absolute
    /// `HOME`.
    pub fn default_dir() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_absolute())
            .map(|h| h.join("Library/Application Support/com.fogar.agent/run"))
    }

    /// Binds `<dir>/agent.sock` for the companion app ([`COMPANION_APP_ID`]).
    pub fn bind(dir: &Path) -> std::io::Result<Self> {
        Self::bind_for(dir, COMPANION_APP_ID)
    }

    /// Binds `<dir>/agent.sock` accepting `peer_identifier` as the same-signer peer. `dir` is
    /// created 0700 and must be ours. A stale socket is removed only if nothing answers on it and
    /// it is a socket we own (never follows symlinks).
    pub fn bind_for(dir: &Path, peer_identifier: &str) -> std::io::Result<Self> {
        if !is_identifier(peer_identifier) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "bad peer identifier",
            ));
        }
        ensure_private_dir(dir)?;
        let path = dir.join("agent.sock");
        if path.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "socket path too long",
            ));
        }
        let uid = euid();
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
        let signer = own_team_id().map(|team| {
            (
                format!(
                    r#"anchor apple generic and certificate leaf[subject.OU] = "{team}" and identifier "{peer_identifier}""#
                ),
                peer_identifier.to_owned(),
            )
        });
        Ok(Self {
            listener,
            path,
            uid,
            signer,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn identify(&self, stream: &UnixStream, uid: u32) -> PeerIdentity {
        if let Some((req, id)) = &self.signer
            && peer_audit_token(stream).is_some_and(|t| token_satisfies(&t, req))
        {
            return PeerIdentity::verified_signer(Some(uid), id.clone());
        }
        PeerIdentity::uid_only(Some(uid))
    }
}

impl Drop for MacIpcServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl IpcServer for MacIpcServer {
    type Conn = MacIpcConn;

    fn capabilities(&self) -> IpcCaps {
        let mut c = IpcCaps::NONE;
        c.peer_uid = Support::Native;
        if self.signer.is_some() {
            c.peer_code_identity = Support::Native;
        }
        c
    }

    async fn accept(&self) -> Result<(PeerIdentity, MacIpcConn), IpcError> {
        loop {
            // Cancel-safe: `UnixListener::accept` is, and nothing below awaits.
            let (stream, _) = self.listener.accept().await.map_err(io_err)?;
            match stream.peer_cred() {
                Ok(c) if c.uid() == self.uid => {
                    // shortcut: the SecCode check runs inline (a few ms, once per UI connect) so
                    // accept stays cancel-safe; move to an owned in-flight slot if measured slow.
                    let peer = self.identify(&stream, c.uid());
                    return Ok((peer, MacIpcConn::new(stream)));
                }
                // Another user (or unknown): drop the connection before reading a byte.
                _ => continue,
            }
        }
    }
}

/// Framed connection. `recv` keeps partial frames in `buf` so it is cancel-safe.
#[derive(Debug)]
pub struct MacIpcConn {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl MacIpcConn {
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

impl IpcConn for MacIpcConn {
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

/// Client side of the wire format (contract tests, the signer probe example, the Swift app's
/// reference).
#[derive(Debug)]
pub struct MacIpcClient {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl MacIpcClient {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requirement_inputs_are_validated() {
        assert!(is_team_id("Q6UYYP85B7"));
        for bad in ["", "q6uyyp85b7", "Q6UYYP85B", "Q6UYYP85B7X", "Q6UYYP85\"7"] {
            assert!(!is_team_id(bad), "{bad:?}");
        }
        assert!(is_identifier("com.fogar.desktop"));
        for bad in ["", "com.fogar\" or always", "a b", "x\n"] {
            assert!(!is_identifier(bad), "{bad:?}");
        }
    }

    /// The token → SecCode → requirement pipeline works on a real connection (this test process
    /// on both ends), and a foreign Team ID never passes.
    #[test]
    fn audit_token_pipeline_checks_the_real_peer() {
        fogar_testkit::run_async(async {
            let (a, _b) = UnixStream::pair().expect("pair");
            let tok = peer_audit_token(&a).expect("LOCAL_PEERTOKEN");
            assert!(
                token_satisfies(&tok, "always"),
                "token must resolve to our SecCode"
            );
            assert!(!token_satisfies(
                &tok,
                r#"anchor apple generic and certificate leaf[subject.OU] = "ABCDE12345" and identifier "com.fogar.desktop""#
            ));
            assert!(
                !token_satisfies(&[0; 8], "always"),
                "a bogus token never passes"
            );
        });
    }
}
