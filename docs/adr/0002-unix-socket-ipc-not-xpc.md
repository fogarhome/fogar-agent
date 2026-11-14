# ADR-0002: Local UI IPC over a Unix domain socket, not XPC

Status: **PROVISIONAL**

## Decision
The Swift UI talks to the agent over a Unix domain socket in a 0700 directory
(`~/Library/Application Support/com.fogar.agent/run/agent.sock` on macOS), 4-byte length-prefixed
JSON frames (max 64 KiB, `deny_unknown_fields`, closed request set: `hello`, `status`), behind the
`IpcServer` port. macOS peers are authenticated with `LOCAL_PEERTOKEN` -> audit token -> SecCode
designated requirement (Team ID); never by PID. XPC stays a possible later adapter behind the same
port.

Why: portable to Linux (UDS + `SO_PEERCRED`) and Windows (named pipes with a user-SID DACL); no
maintained Rust XPC crate (hand FFI to libxpc); fogar-macos-daemon §1.

## State
- Linux adapter implemented and contract-tested (uid check only).
- macOS adapter (S1): uid gate + same-signer check, contract-tested on the macOS runner and the M2.

## To verify on the M2
Results 2026-11-14 on the M2 (macOS 26.5.1), `crates/fogar-agent-macos/examples/signer_probe.rs`
with separately signed processes, Personal Team identity "Apple Development" (Team ID read from
the signature, not the name in parentheses of the certificate):
- [x] `LOCAL_PEERTOKEN` yields the peer's audit token (server side; also from a socketpair in the
  unit test). Client side (UI verifying the agent) NOT tested: that is the Swift app's job.
- [x] `SecCodeCheckValidity` with `anchor apple generic and certificate leaf[subject.OU] =
  "<TEAM>" and identifier "com.fogar.desktop" and !(entitlement[allow-dyld-environment-variables]
  exists) and !(entitlement[disable-library-validation] exists)`, plus the peer's dynamic status
  must carry `CS_RUNTIME` (hardened runtime; with those two entitlements banned, a same-user
  process cannot load a dylib into the genuine app). The app MUST be built with "Enable Hardened
  Runtime" and without those entitlements. Same Personal Team + `com.fogar.desktop` +
  `-o runtime` → `SameSigner`; same without the hardened runtime → `UidOnly`; same with
  `disable-library-validation` → `UidOnly`; client ad-hoc → `UidOnly`; same signer but id `com.otro` or
  `com.fogar.agent` → `UidOnly`; ad-hoc agent → caps `peer_code_identity: Unsupported` and
  `UidOnly` for every client. Check cost 1–8 ms, run on the blocking pool with a 2 s deadline
  (timeout = `UidOnly`), one check in flight; a timed-out check that still runs blocks new checks
  (peers get `UidOnly`) so at most one thread is ever stuck; `accept` stays cancel-safe.
- **`LOCAL_PEERTOKEN` is not the connector's identity** (found by an independent review,
  reproduced on the M2 2026-11-14): the kernel answers with the process that did the LAST I/O on
  the peer socket, in its current image. Attack: an unsigned same-user process connects, forks
  (the child keeps the fd) and execs the genuine signed app; the agent verifies the app
  (`SameSigner`), then the child writes `Pair` — or the parent writes `Pair` before the exec. Both
  were delivered before the fix. Fix in the adapter (no port change): on a verified connection a
  privileged request (anything but `Hello`/`Status`/`Subscribe`/`Discover`) is delivered only if
  none of its bytes were queued before the verification (FIONREAD read right after the token),
  the token read after the frame is still the verified one, and nothing else is queued or
  buffered behind it; otherwise the connection is closed. After the fix both attacks end in
  "connection closed" (3/3 and 2/2 runs) and the app's own `Pair` sent after its `Hello` reply is
  delivered. Recipe: `examples/signer_probe.rs` header (attack script in the PR). Consequences:
  the app must wait for the `Hello` reply before `Pair`/`Unpair` and never pipeline after them.
  Residual: a Unix socket has no per-message identity. The check fails only if the genuine app
  itself writes (possibly a zero-length write, not measured) after the attacker's frame and before
  the agent reads the token; the attacker controls the app's inputs but not its code. Closing
  that completely needs per-request proof (XPC, or a MAC with a key only the app holds): a
  decision for S7 / the orchestrator, not a port change (a per-request verdict has the same
  attribution problem).
- A signed agent whose own signature cannot be read or no longer validates refuses to bind (fails
  closed) instead of downgrading to uid-only. Not exercised: a tampered bare executable is killed
  by the kernel before it runs; a tampered `.app` bundle resource is the case this covers.
- Accepted risks (Personal Team, S1): Xcode Debug builds carry `get-task-allow`, so a same-user
  debugger can attach to the genuine app; and anyone who can use the owner's Apple Development
  private key (same user, if the keychain lets `codesign` use it without a prompt) can sign a
  `com.fogar.desktop` that passes. Both reduce to "the owner's account is already compromised".
  S7 (Developer ID): drop `get-task-allow` from release builds and consider requiring its absence.
- [ ] Developer ID builds (S7) and a real `.app` bundle peer (the probe signs bare executables;
  bundle resource validation may cost more): re-run the probe recipe then.
- [x] Socket path: `~/Library/Application Support/com.fogar.agent/run/agent.sock` is 78 bytes for
  this home; `bind` refuses > 103 bytes (tested).

## Amendment (2026-11-14): no paid Apple Developer account
The owner has no paid account, so no fixed Developer ID Team ID can be pinned. The macOS peer check
requires the peer to be signed by **the same signer as the agent itself** (read from the agent's
own code signature at start-up) plus the expected bundle identifier. This works with a free Xcode
Personal Team now and with Developer ID later, without code changes. An unsigned or ad-hoc-signed
agent falls back to the uid check (as on Linux) and reports it in `status`. Developer ID and
notarization are deferred to S7. To verify on the M2: Personal Team signatures yield a usable
designated requirement for this comparison. Verified 2026-11-14 (see above): yes, `subject.OU` of an "Apple
Development" leaf equals the Personal Team ID; the app must be signed with the same identity as the
agent. Keychain consequence (measured): items written by an agent are readable and updatable by a
rebuild signed by the same identity but not deletable (`delete` writes a tombstone instead); an
ad-hoc rebuild cannot read them (`security delete-generic-password -s com.fogar.agent -a
gateway_pairing` recovers).
