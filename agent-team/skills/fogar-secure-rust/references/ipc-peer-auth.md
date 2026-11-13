# Local IPC: Unix socket between daemon and local UI

Labels: [primary] checked 2026-11-13 in crate source; [secondary]; [unverified].

## Threat

Any process running as the same user can `connect()` to a socket it can reach. Uid checks alone
therefore only stop *other users*. A malicious same-user app (or a compromised browser helper)
must be stopped by **code identity** of the peer if the socket exposes mic/screen/SSH/secrets.

## Required layers

1. **Filesystem**: socket inside a private directory (`0700`, owned by the agent user), e.g. under
   the user's Application Support or `$TMPDIR`-derived private dir; socket file `0600`. Create the
   directory with `DirBuilder` + mode, verify owner/mode with `symlink_metadata` on the *opened*
   handle where possible; refuse to start if the dir is a symlink or has wrong owner.
2. **Peer uid**: `tokio::net::UnixStream::peer_cred()` → `UCred { uid(), gid(), pid() }` [primary,
   tokio 1.54.1]. On macOS tokio uses `getpeereid` for uid/gid and `LOCAL_PEEREPID` for pid
   [primary: tokio `src/net/unix/ucred.rs`]. Reject if `uid != geteuid()`.
3. **Peer code identity (macOS)**: get the peer **audit token** with `nix` 0.31
   `sys::socket::sockopt::LocalPeerToken` (apple targets) [primary], then
   `security-framework` 3.7 `os::macos::code_signing::{GuestAttributes, SecCode, SecRequirement}`:
   `GuestAttributes::set_audit_token`, `SecCode::copy_guest_with_attribues` (sic, crate spelling),
   `SecCode::check_validity(flags, &requirement)` [primary]. Requirement string pins our Team ID and
   bundle identifier, e.g. `anchor apple generic and certificate leaf[subject.OU] = "<TEAMID>" and identifier "<ui-bundle-id>"` [secondary: requirement language].
4. **Why not PID**: a PID can be recycled between the check and use; audit tokens include a PID
   version and are the documented way to identify a peer [secondary]. `LocalPeerPid` exists in nix
   but is advisory only.
5. **Re-check on reconnect**: identity is per connection; never cache "trusted" across sockets.

## Protocol rules

- Length-prefixed frames, max size (e.g. 64 KiB control, separate bulk channel if needed).
- Messages are a closed `enum` with `#[serde(deny_unknown_fields)]`; version field; unknown → close.
- Authorization per method (e.g. "start capture" requires the UI's code identity *and* a user
  gesture flag set by the UI; "read secret" is not an IPC method at all).
- Timeouts on read; limit concurrent connections; one bad frame closes the connection.
- Responses never include secrets or raw capture data unless that is the method's purpose and the
  peer identity is verified.

## Stale-socket TOCTOU

On startup, if the socket path exists: `symlink_metadata` → must be a socket, owned by us, in our
private dir → then `remove_file`. Never `remove_file` on a path inside a shared directory; never
follow symlinks. Race-free alternative: create a fresh private dir per run.

## Windows/Linux later

Linux: `SO_PEERCRED` (tokio `peer_cred` covers it [secondary]); code identity has no direct
equivalent — compare `/proc/<pid>/exe` is racy [secondary]. Windows: named pipes with explicit DACL
and `GetNamedPipeClientProcessId` + signature check [unverified]. Re-run this checklist per OS.

## Review questions

- Can a process of another user connect? (dir/file modes)
- Can an unsigned same-user process connect and invoke a sensitive method? (code requirement)
- Is any identity taken from the message body?
- What is the largest allocation a peer can trigger?
