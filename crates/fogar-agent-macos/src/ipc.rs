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
//!
//! `LOCAL_PEERTOKEN` names the process that did the LAST I/O on the peer socket, in its CURRENT
//! image (pid version is re-read: an exec shows the new program), not the process that
//! connected. So a verdict at accept says nothing about who writes later: an unsigned process can
//! connect, fork (the child keeps the fd), exec the genuine app and let the agent verify the app,
//! then write from the child; or write a request and exec the app before the agent looks
//! (reproduced on the M2, ADR-0002). A Unix socket has no per-message identity, so on a verified
//! connection every request that needs it (anything but `Hello`/`Status`/`Subscribe`/`Discover`)
//! must be attributable to the verified process, else the connection is closed: none of its bytes
//! were queued before the verification, the token read after the frame is still the verified one
//! (the verified process wrote last) and nothing else is queued or buffered behind it (nobody else
//! wrote after it). The companion app therefore sends `Pair`/`Unpair` only after its `Hello` was
//! answered, and waits for each answer before writing again.
//! This is a mitigation, not per-message identity: a write that blocked before the verification,
//! or I/O by the genuine app on an inherited fd, defeats it (open residuals, ADR-0002).

use core_foundation::{
    base::{CFType, TCFType},
    data::CFData,
    dictionary::{CFDictionary, CFDictionaryRef},
    number::CFNumber,
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
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::Mutex,
    task::JoinSet,
    time::Instant,
};
use zeroize::Zeroizing;

/// `sun_path` is 104 bytes on macOS (incl. NUL).
const MAX_SOCKET_PATH: usize = 103;

/// Bundle identifier of the companion app (`apps/macos/`). GUESSED (S1 spec: "Bundle id
/// `com.fogar.desktop` (GUESSED)"); frozen once the app ships.
pub const COMPANION_APP_ID: &str = "com.fogar.desktop";

/// `kSecCSSigningInformation` / `kSecCSDynamicInformation` (`<Security/SecCode.h>`).
const K_SEC_CS_SIGNING_INFORMATION: u32 = 1 << 1;
const K_SEC_CS_DYNAMIC_INFORMATION: u32 = 1 << 3;
/// `CS_RUNTIME` (`<kern/cs_blobs.h>`): the process runs with the hardened runtime. Together with
/// the requirement's ban on the `allow-dyld-environment-variables` and
/// `disable-library-validation` entitlements, another same-user process cannot load code into it.
const CS_RUNTIME: u32 = 0x0001_0000;
/// Upper bound for one peer check (SecCode talks to securityd and may validate a bundle on disk).
const PEER_CHECK_DEADLINE: Duration = Duration::from_secs(2);

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    /// `<Security/SecCode.h>`; accepts a `SecCodeRef` where a `SecStaticCodeRef` is expected.
    fn SecCodeCopySigningInformation(
        code: *const std::ffi::c_void,
        flags: u32,
        information: *mut CFDictionaryRef,
    ) -> i32;
    static kSecCodeInfoTeamIdentifier: CFStringRef;
    static kSecCodeInfoStatus: CFStringRef;
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

/// `SecCodeCopySigningInformation(code, flags)`; `None` on failure.
fn signing_info(code: &SecCode, flags: u32) -> Option<CFDictionary<CFString, CFType>> {
    let mut info: CFDictionaryRef = std::ptr::null();
    // SAFETY: `code` is a live SecCodeRef for the duration of the call (Apple documents SecCodeRef
    // as accepted where a SecStaticCodeRef is expected); `info` is a valid out-pointer; on success
    // we own the returned dictionary (Copy rule) and wrap it exactly once below.
    let rc = unsafe {
        SecCodeCopySigningInformation(code.as_concrete_TypeRef().cast(), flags, &mut info)
    };
    if rc != 0 || info.is_null() {
        return None;
    }
    // SAFETY: non-null CFDictionaryRef returned by a Copy function: create rule, released on drop.
    Some(unsafe { CFDictionary::wrap_under_create_rule(info) })
}

/// Looks up a Security.framework `kSecCodeInfo*` key; `None` if the symbol is null or absent.
fn info_value(dict: &CFDictionary<CFString, CFType>, key: CFStringRef) -> Option<CFType> {
    if key.is_null() {
        return None;
    }
    // SAFETY: non-null immutable CFString constant exported by Security.framework; the get rule
    // retains it for the wrapper's lifetime.
    let key = unsafe { CFString::wrap_under_get_rule(key) };
    dict.find(&key).map(|v| v.clone())
}

fn bind_error(msg: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, msg)
}

/// The Team ID of the running agent: `Ok(None)` when it carries no Team ID (ad-hoc/unsigned:
/// uid-only, as on Linux). Fails closed: a signature we cannot read, or a Team-ID signature that
/// no longer validates (tampered bundle), is an error, never a silent downgrade to uid-only.
fn own_team_id() -> std::io::Result<Option<String>> {
    let me = SecCode::for_self(Flags::NONE)
        .map_err(|_| bind_error("cannot read the agent's own code signature"))?;
    let info = signing_info(&me, K_SEC_CS_SIGNING_INFORMATION)
        .ok_or_else(|| bind_error("cannot read the agent's own signing information"))?;
    // SAFETY: loads an extern static `CFStringRef` that Security.framework initialises before any
    // of its functions can be called and never mutates; `info_value` null-checks it.
    let key = unsafe { kSecCodeInfoTeamIdentifier };
    let Some(team) = info_value(&info, key) else {
        return Ok(None);
    };
    let team = team
        .downcast::<CFString>()
        .map(|t| t.to_string())
        .filter(|t| is_team_id(t))
        .ok_or_else(|| bind_error("unexpected Team ID in the agent's signature"))?;
    // The agent itself must satisfy the team requirement it will demand of peers.
    let req = format!(r#"anchor apple generic and certificate leaf[subject.OU] = "{team}""#)
        .parse()
        .map_err(|_| bind_error("cannot build the team requirement"))?;
    me.check_validity(Flags::NONE, &req)
        .map_err(|_| bind_error("the agent's own signature does not validate"))?;
    Ok(Some(team))
}

/// Bytes queued in our receive buffer, or `None` on error.
fn queued_bytes(stream: &UnixStream) -> Option<usize> {
    let mut n: libc::c_int = 0;
    // SAFETY: the fd is a live socket owned by `stream`; FIONREAD writes one `int` through the
    // pointer, which points to a valid, aligned, writable `c_int` that outlives the call.
    let rc = unsafe { libc::ioctl(stream.as_raw_fd(), libc::FIONREAD, &mut n) };
    (rc == 0).then(|| usize::try_from(n).ok()).flatten()
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
/// Requires the hardened runtime too (see [`CS_RUNTIME`]).
fn token_satisfies(token: &[u32; 8], requirement: &str) -> bool {
    let Some(code) = guest_code(token) else {
        return false;
    };
    satisfies(&code, requirement) && hardened_runtime(&code)
}

/// The running code identified by an audit token (pid + pid version: no PID reuse; it is the
/// process that last did I/O on the peer socket, in its current image).
fn guest_code(token: &[u32; 8]) -> Option<SecCode> {
    let bytes: Vec<u8> = token.iter().flat_map(|v| v.to_ne_bytes()).collect();
    let data = CFData::from_buffer(&bytes);
    let mut attrs = GuestAttributes::new();
    // `data` outlives `attrs` (the dictionary retains it anyway).
    attrs.set_audit_token(data.as_concrete_TypeRef());
    SecCode::copy_guest_with_attribues(None, &attrs, Flags::NONE).ok()
}

fn satisfies(code: &SecCode, requirement: &str) -> bool {
    let Ok(req) = requirement.parse() else {
        return false;
    };
    code.check_validity(Flags::NONE, &req).is_ok()
}

/// The running peer has the hardened runtime (dynamic `kSecCodeInfoStatus` has `CS_RUNTIME`).
fn hardened_runtime(code: &SecCode) -> bool {
    // SAFETY: loads an extern static `CFStringRef` that Security.framework initialises before any
    // of its functions can be called and never mutates; `info_value` null-checks it.
    let key = unsafe { kSecCodeInfoStatus };
    signing_info(code, K_SEC_CS_DYNAMIC_INFORMATION)
        .and_then(|d| info_value(&d, key))
        .and_then(|v| v.downcast::<CFNumber>())
        .and_then(|n| n.to_i64())
        .is_some_and(|status| status as u32 & CS_RUNTIME != 0)
}

/// A connection whose code-signature check runs on the blocking pool. Kept in the server so a
/// cancelled `accept` resumes it instead of losing the connection.
#[derive(Debug)]
struct InFlight {
    stream: UnixStream,
    uid: u32,
    deadline: Instant,
    check: Option<JoinSet<bool>>,
    /// The token the check runs on and the bytes already queued right after reading it.
    seen: Option<Attribution>,
}

#[derive(Debug)]
pub struct MacIpcServer {
    listener: UnixListener,
    path: PathBuf,
    /// `(dev, ino)` of the socket we bound: `Drop` never removes someone else's socket.
    inode: (u64, u64),
    uid: u32,
    /// `Some` only when the agent is signed with a certificate: (requirement, expected id).
    signer: Option<(String, String)>,
    /// At most one peer check in flight; connections queue in the kernel backlog meanwhile.
    in_flight: Mutex<Slot>,
    /// [`PEER_CHECK_DEADLINE`]; shorter only in tests.
    check_deadline: Duration,
}

#[derive(Debug, Default)]
struct Slot {
    pending: Option<InFlight>,
    /// A check that missed its deadline. A blocking task cannot be aborted, so while it still
    /// runs no new check is started (peers get `UidOnly`): at most one blocked thread, ever.
    stuck: Option<JoinSet<bool>>,
}

impl MacIpcServer {
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
        let signer = own_team_id()?.map(|team| {
            (
                format!(
                    r#"anchor apple generic and certificate leaf[subject.OU] = "{team}" and identifier "{peer_identifier}" and !(entitlement["com.apple.security.cs.allow-dyld-environment-variables"] exists) and !(entitlement["com.apple.security.cs.disable-library-validation"] exists)"#
                ),
                peer_identifier.to_owned(),
            )
        });
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let md = std::fs::symlink_metadata(&path)?;
        Ok(Self {
            listener,
            path,
            inode: (md.dev(), md.ino()),
            uid,
            signer,
            in_flight: Mutex::new(Slot::default()),
            check_deadline: PEER_CHECK_DEADLINE,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accepts the next same-uid connection; with a signer policy, starts its peer check on the
    /// blocking pool (`check: None` = no policy, uid-only).
    async fn next_candidate(
        &self,
        stuck: &mut Option<JoinSet<bool>>,
    ) -> Result<InFlight, IpcError> {
        loop {
            // Cancel-safe: `UnixListener::accept` is.
            let (stream, _) = self.listener.accept().await.map_err(io_err)?;
            let uid = match stream.peer_cred() {
                Ok(c) if c.uid() == self.uid => c.uid(),
                // Another user (or unknown): drop the connection before reading a byte.
                _ => continue,
            };
            if let Some(set) = stuck {
                while set.try_join_next().is_some() {}
                if set.is_empty() {
                    *stuck = None;
                }
            }
            let mut seen = None;
            let check = self
                .signer
                .as_ref()
                .filter(|_| stuck.is_none())
                .map(|(req, _)| {
                    let mut set = JoinSet::new();
                    // Token first, then the queue: bytes written by anyone before the token was
                    // read (e.g. before an exec) are counted as not attributable.
                    let tok = peer_audit_token(&stream);
                    let queued = queued_bytes(&stream);
                    seen = tok.zip(queued).map(|(token, unattributed)| Attribution {
                        token,
                        unattributed,
                    });
                    let req = req.clone();
                    let ok = seen.is_some();
                    set.spawn_blocking(move || {
                        ok && tok.is_some_and(|t| token_satisfies(&t, &req))
                    });
                    set
                });
            return Ok(InFlight {
                stream,
                uid,
                deadline: Instant::now() + self.check_deadline,
                check,
                seen,
            });
        }
    }
}

impl Drop for MacIpcServer {
    fn drop(&mut self) {
        let ours = std::fs::symlink_metadata(&self.path)
            .is_ok_and(|md| (md.dev(), md.ino()) == self.inode);
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
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
        // Cancel-safe: the accepted connection and its pending check live in `self.in_flight`, so
        // a dropped `accept` resumes them on the next call.
        let mut guard = self.in_flight.lock().await;
        let slot = &mut *guard;
        if slot.pending.is_none() {
            slot.pending = Some(self.next_candidate(&mut slot.stuck).await?);
        }
        let verified = match slot
            .pending
            .as_mut()
            .and_then(|f| f.check.as_mut().map(|c| (f.deadline, c)))
        {
            // A check that misses the deadline (or panics) counts as "not verified".
            Some((deadline, check)) => {
                match tokio::time::timeout_at(deadline, check.join_next()).await {
                    Ok(r) => matches!(r, Some(Ok(true))),
                    Err(_) => false,
                }
            }
            None => false,
        };
        let Some(mut f) = slot.pending.take() else {
            // Unreachable: `pending` was filled above and only taken here.
            return Err(IpcError::Transport("accept slot empty".into()));
        };
        if let Some(check) = f.check.take().filter(|c| !c.is_empty()) {
            slot.stuck = Some(check);
        }
        match (&self.signer, verified, f.seen) {
            (Some((_, id)), true, Some(seen)) => Ok((
                PeerIdentity::verified_signer(Some(f.uid), id.clone()),
                MacIpcConn::new(f.stream, Some(seen)),
            )),
            _ => Ok((
                PeerIdentity::uid_only(Some(f.uid)),
                MacIpcConn::new(f.stream, None),
            )),
        }
    }
}

/// Framed connection. `recv` keeps partial frames in `buf` so it is cancel-safe.
#[derive(Debug)]
pub struct MacIpcConn {
    stream: UnixStream,
    /// Wiped on drop: a half-received `Pair` request carries the pairing code.
    buf: Zeroizing<Vec<u8>>,
    /// `Some` on a verified (`SameSigner`) connection: who may send privileged requests.
    verified: Option<Attribution>,
}

/// What a verified connection's privileged requests are checked against (module docs).
#[derive(Debug, Clone, Copy)]
struct Attribution {
    /// The verified process's audit token.
    token: [u32; 8],
    /// Stream bytes queued before the verification: whoever wrote them is unknown.
    unattributed: usize,
}

/// Requests any same-uid peer may send (the core answers them for `UidOnly` too).
fn needs_no_attribution(r: &IpcRequest) -> bool {
    matches!(
        r,
        IpcRequest::Hello { .. }
            | IpcRequest::Status
            | IpcRequest::Subscribe
            | IpcRequest::Discover
    )
}

impl MacIpcConn {
    fn new(stream: UnixStream, verified: Option<Attribution>) -> Self {
        Self {
            stream,
            buf: Zeroizing::new(Vec::with_capacity(512)),
            verified,
        }
    }

    /// Whether the frame just taken (`frame_len` stream bytes) came from the verified process
    /// alone. Order matters: token first (the verified process wrote last), then the queue and
    /// the buffer (nobody wrote after it).
    fn attributable(&mut self, frame_len: usize) -> bool {
        let Some(v) = self.verified.as_mut() else {
            return true;
        };
        let before_verification = v.unattributed > 0;
        v.unattributed = v.unattributed.saturating_sub(frame_len);
        let token = v.token;
        !before_verification
            && peer_audit_token(&self.stream) == Some(token)
            && queued_bytes(&self.stream) == Some(0)
            && self.buf.is_empty()
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
        // Synchronous from here on: no await between taking the frame and checking it.
        let attributable = self.attributable(4 + body.len());
        let req = decode_request(&body)?;
        if self.verified.is_some() && !attributable && !needs_no_attribution(&req) {
            return Err(IpcError::Transport(
                "privileged request not attributable to the verified peer".into(),
            ));
        }
        Ok(Some(req))
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
            let me = guest_code(&tok).expect("token must resolve to our SecCode");
            assert!(satisfies(&me, "always"));
            assert!(!satisfies(
                &me,
                r#"anchor apple generic and certificate leaf[subject.OU] = "ABCDE12345" and identifier "com.fogar.desktop""#
            ));
            // `cargo test` binaries lack the hardened runtime: never verified, even for "always".
            assert!(!hardened_runtime(&me));
            assert!(!token_satisfies(&tok, "always"));
            assert!(
                guest_code(&[0; 8]).is_none(),
                "a bogus token never resolves"
            );
        });
    }

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("fogar-unit-{tag}-{}", std::process::id()))
    }

    /// The signed-agent path (`cargo test` binaries are unsigned, so force a policy): the check
    /// runs on the blocking pool, a cancelled `accept` resumes the same connection, a missed
    /// deadline is `UidOnly` and never loses the next connection.
    #[test]
    fn in_flight_check_survives_cancel_and_deadline() {
        fogar_testkit::run_async(async {
            let mut server = MacIpcServer::bind(&scratch("inflight")).expect("bind");
            server.signer = Some(("always".into(), COMPANION_APP_ID.into()));
            let path = server.path().to_path_buf();
            let _c1 = MacIpcClient::connect(&path).await.expect("connect");
            // Cancel accept repeatedly while the check may still be running.
            for _ in 0..5 {
                let _ = tokio::time::timeout(Duration::ZERO, server.accept()).await;
            }
            let (peer, _conn) = server.accept().await.expect("accept");
            // "always" passes the requirement, but this binary lacks the hardened runtime.
            assert_eq!(peer.check(), fogar_ports::ipc::PeerCheck::UidOnly);
            assert!(server.in_flight.lock().await.pending.is_none());

            server.check_deadline = Duration::ZERO;
            let _c2 = MacIpcClient::connect(&path).await.expect("connect");
            let _c3 = MacIpcClient::connect(&path).await.expect("connect");
            for _ in 0..2 {
                let (peer, _conn) = tokio::time::timeout(Duration::from_secs(5), server.accept())
                    .await
                    .expect("no connection lost")
                    .expect("accept");
                assert_eq!(peer.check(), fogar_ports::ipc::PeerCheck::UidOnly);
            }
        });
    }

    /// A verified connection over a socketpair: this process is the verified peer.
    fn verified_pair(
        queued_before: bool,
        pre: &[u8],
    ) -> (MacIpcConn, std::os::unix::net::UnixStream) {
        use std::io::Write;
        let (srv, mut cli) = std::os::unix::net::UnixStream::pair().expect("pair");
        cli.write_all(pre).expect("write");
        srv.set_nonblocking(true).expect("nonblocking");
        let srv = UnixStream::from_std(srv).expect("tokio stream");
        let token = peer_audit_token(&srv).expect("token");
        let unattributed = if queued_before {
            queued_bytes(&srv).expect("FIONREAD")
        } else {
            0
        };
        (
            MacIpcConn::new(
                srv,
                Some(Attribution {
                    token,
                    unattributed,
                }),
            ),
            cli,
        )
    }

    fn wire(r: &IpcRequest) -> Vec<u8> {
        frame(&encode_request(r).expect("encode"))
            .expect("frame")
            .to_vec()
    }

    fn pair_req() -> IpcRequest {
        IpcRequest::Pair {
            address: "evil:8004".into(),
            code: fogar_ports::ipc::PairCode::parse("482913"),
        }
    }

    /// ADR-0002 attack: whoever shares the verified connection's fd cannot send a privileged
    /// request; the verified process itself still can (also after a pipelined `Hello`).
    #[test]
    fn privileged_requests_must_come_from_the_verified_process() {
        use std::io::Write;
        fogar_testkit::run_async(async {
            let hello = wire(&IpcRequest::Hello { version: 2 });

            // Legit: Hello sent before the verification, Pair after it, both by us.
            let (mut conn, mut cli) = verified_pair(true, &hello);
            assert!(matches!(
                conn.recv().await,
                Ok(Some(IpcRequest::Hello { .. }))
            ));
            cli.write_all(&wire(&pair_req())).expect("write");
            assert!(matches!(
                conn.recv().await,
                Ok(Some(IpcRequest::Pair { .. }))
            ));

            // Another process (cat) writes on the shared fd: refused, connection closes.
            let (mut conn, cli) = verified_pair(false, &[]);
            let f = std::env::temp_dir().join(format!("fogar-unit-frame-{}", std::process::id()));
            std::fs::write(&f, wire(&pair_req())).expect("frame file");
            let out = std::process::Stdio::from(std::os::fd::OwnedFd::from(
                cli.try_clone().expect("dup"),
            ));
            let st = std::process::Command::new("/bin/cat")
                .arg(&f)
                .stdout(out)
                .status()
                .expect("cat");
            assert!(st.success());
            let _ = std::fs::remove_file(&f);
            assert!(
                conn.recv().await.is_err(),
                "a frame written by cat must be refused"
            );
            // A harmless request from it would still pass (the core answers it to anyone).

            // Written before the verification (the pre-exec parent): refused.
            let (mut conn, _cli) = verified_pair(true, &wire(&pair_req()));
            assert!(conn.recv().await.is_err());

            // Something queued behind the privileged frame: nobody can tell who wrote last.
            let mut both = wire(&pair_req());
            both.extend(wire(&IpcRequest::Status));
            let (mut conn, mut cli) = verified_pair(false, &[]);
            cli.write_all(&both).expect("write");
            assert!(conn.recv().await.is_err());
            drop(cli);
        });
    }
}
