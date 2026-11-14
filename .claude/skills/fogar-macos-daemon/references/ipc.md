# IPC between the SwiftUI app and fogar-agent

Untested on macOS. Tags as in launchd-smappservice.md.

## 1. Recommendation

**v1: a Unix domain socket (stream) for the whole protocol. Each side checks the other's code signature through `LOCAL_PEERTOKEN` → `SecCode` designated requirement. Hide it behind an `IpcTransport` port so XPC can be added later as a second macOS adapter without touching core.**

| Criterion | Unix domain socket | XPC (libxpc / NSXPC) |
|---|---|---|
| Rust support today | tokio `UnixListener`, `interprocess` 2.4.8 local sockets [primary] | **No maintained crate.** `xpc-connection` 0.2.3 (2020) is client-only on the old `block` crate. `xpc-sys` 1.0.0 has ~14 recent downloads. objc2 has **no XPC framework crate** [primary: crates.io + objc2 repo listing]. `objc2-foundation` has `NSXPCConnection`/`NSXPCListener` (`initWithMachServiceName`, `setConnectionCodeSigningRequirement`, `setCodeSigningRequirement`) [primary], but NSXPC needs an Objective-C protocol with **extended block type encodings**. objc2's `ProtocolBuilder` cannot emit those, so reply blocks probably will not work [unverified, high risk]. The realistic route is hand-written FFI to the libxpc C API (`xpc_connection_create_mach_service` + listener flag, `xpc_connection_set_peer_code_signing_requirement`, dictionaries carrying one `data` blob) + `block2` handlers + `dispatch2` queues. That is moderate pain: about 500 lines of unsafe code and Mac-only tests |
| Peer identity | Audit token of the peer **at connect time**. "it tells you who created the socket, not who sent the message… it's not as secure as XPC" (Quinn) [secondary: forum 775267] | Per-message audit token, plus a built-in code-signing requirement check [secondary] |
| Launch on demand / Adaptive ProcessType | No (launchd `Sockets` exists but needs launch_activate_socket; not needed with KeepAlive) | Yes (`MachServices`). Quinn: "To listen on a named XPC endpoint, you must be started by launchd with a configuration that includes that endpoint name" [secondary] |
| Portability | Same code on Linux. Windows uses named pipes behind the same port; `interprocess` abstracts both | macOS only |
| Testability | Linux CI can run the full protocol tests | Mac-only |
| Apple's preference | "Unix domain sockets aren't a terrible idea, but they're substantially less flexible than XPC" [secondary] | Preferred by DTS |

When to add XPC: if we need Adaptive ProcessType boosts during UI requests, launch-on-demand, or per-message identity (for example, a third-party process could get our fd). Same-user attackers already own the user account, so for v1 the connect-time check is accepted. **Record this as an explicit risk.**

## 2. Socket location and permissions

- Path: `~/Library/Application Support/com.fogar.agent/run/agent.sock`. Create the directory `0700` and the socket `0600`. The agent runs as the user, so `getpeereid` uid must equal `geteuid()`.
- `sun_path` is **104 bytes** (`char sun_path[104]` in xnu `bsd/sys/un.h`) [primary]. Quinn: the kernel accepts up to 253 bytes with "gnarly pointer play", and `NSTemporaryDirectory()` has no length guarantee [secondary: forum 817602]. Rule: compute the path, and **fail fast with a clear error if it exceeds 103 bytes** (long usernames). Fallback: a short symlinked dir such as `/tmp/fogar-<uid>/` with 0700 permissions and an ownership check [unverified safety, review].
- App group containers (`~/Library/Group Containers/<TEAMID>.group…`) are only needed if the UI app becomes sandboxed. macOS 15 added container-protection prompts tied to app-group authorisation (Quinn's "App Groups: macOS vs iOS: Fight!") [secondary]. **v1: keep the UI app non-sandboxed (Developer ID) and avoid app groups.**
- Remove a stale socket on startup only after `connect()` to it fails (otherwise a second instance would steal it). Hold an `flock` on `run/agent.lock` to enforce a single instance.

## 3. Peer verification (macOS adapter)

Both directions:
- **Daemon verifies client:** must be our UI app (or our CLI).
- **UI verifies daemon:** protects against squatting on the socket path.

`getsockopt` on a connected socket returns the *peer's* token on either side [unverified for the client side, test it].

```rust
// macOS adapter only. Names verified in libc 0.2.191 / objc2-security 0.3.2; untested.
// audit_token_t = struct { unsigned int val[8]; } (xnu osfmk/mach/message.h) — libc has no type for it.
let mut tok = [0u32; 8];
let mut len = std::mem::size_of_val(&tok) as libc::socklen_t;
let rc = unsafe { libc::getsockopt(fd, libc::SOL_LOCAL, libc::LOCAL_PEERTOKEN,
                                   tok.as_mut_ptr().cast(), &mut len) };
// rc != 0 => reject.
// Then (objc2-security, feature "CSCommon" + "SecCode" + "SecRequirement"):
//   attrs = CFDictionary { kSecGuestAttributeAudit : CFData(bytes of tok) }
//   SecCode::copy_guest_with_attributes(None, Some(&attrs), SecCSFlags::DefaultFlags, &mut code)
//   SecRequirement::create_with_string(&req_text, SecCSFlags::DefaultFlags, &mut req)
//   code.check_validity(SecCSFlags::DefaultFlags, Some(&req)) == errSecSuccess
```
- Requirement text (TN3127 style): `anchor apple generic and certificate leaf[subject.OU] = "<TEAMID>" and identifier "com.fogar.app"`. For dev builds, use a separate requirement behind a debug-only cfg. Quinn: "Don't use bundle IDs for anything related to security"; use the designated requirement (TN3127) [secondary: forum 760972].
- Do **not** use the PID (`LOCAL_PEERPID`, tokio `peer_cred().pid()`): PID reuse [secondary]. tokio's macOS `peer_cred` uses `LOCAL_PEEREPID` + `getpeereid`, so there is no audit token [primary: tokio 1.54.1 source]. `interprocess` `PeerCreds` reads `xucred` (uid/groups) and has no audit token either [primary]. **You must do the getsockopt yourself.**
- Run the SecCode check off the async executor (`spawn_blocking`). It can touch disk.

## 4. Wire protocol (lives in a `fogar-protocol` crate, platform-free)

- Framing: u32 big-endian length prefix with a max frame size (for example 1 MiB). Reject larger frames.
- First frame is a mandatory `Hello { protocol_version, app_version, build }`. The daemon answers `HelloAck { protocol_version, agent_version, capabilities }`. **On a major mismatch the daemon refuses the connection.** This also feeds the update flow (update.md).
- Encoding: JSON (serde_json) in v1 for debuggability. Swift types are generated (swift-interop.md). Switch to a binary codec only after measuring.
- Request IDs, server push events (status, permission changes, power state) and a bounded per-connection queue. Drop slow clients rather than grow memory.
- Idle cost: no heartbeats while no client is connected. Use event-driven accept only.

## 5. Windows and Linux

| | Linux | Windows |
|---|---|---|
| Transport | UDS at `$XDG_RUNTIME_DIR/fogar/agent.sock` (tmpfs, per-user, 0700) | Named pipe `\\.\pipe\fogar-agent-<user SID>` |
| Peer check | `SO_PEERCRED` (uid/pid). Optionally check `/proc/<pid>/exe` against the install path (racy, advisory) | Security descriptor granting only the current user SID; `GetNamedPipeClientProcessId` → path/Authenticode check (advisory, PID race) [unverified details] |
| Hardening flags | — | tokio `ServerOptions::first_pipe_instance(true)` (anti-squatting) and `reject_remote_clients(true)` [primary: tokio 1.54.1 source]; `create_with_security_attributes_raw` for the DACL |
| Rust | tokio / interprocess | tokio `net::windows::named_pipe` / interprocess `local_socket` |

Porting cost: **low**. The framing and protocol crates are shared. Each OS adapter only implements `bind/accept/connect + verify_peer() -> PeerIdentity`. Windows also supports AF_UNIX, but its peer identity story is weaker [unverified], so prefer named pipes.
