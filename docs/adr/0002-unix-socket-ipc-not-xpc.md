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

## Slice 0 state
- Linux adapter implemented and contract-tested (uid check only).
- macOS adapter is a stub (`accept` never resolves): no UI IPC on macOS until S1 (reordered 2026-11-14; was S4).

## To verify on the M2
- `LOCAL_PEERTOKEN` yields the peer's audit token for a client connecting from the signed app.
- `SecCodeCheckValidity` passes for Developer ID and dev builds with the intended requirement.
- Socket path stays < 104 bytes for real home directories.

## Amendment (2026-11-14): no paid Apple Developer account
The owner has no paid account, so no fixed Developer ID Team ID can be pinned. The macOS peer check
requires the peer to be signed by **the same signer as the agent itself** (read from the agent's
own code signature at start-up) plus the expected bundle identifier. This works with a free Xcode
Personal Team now and with Developer ID later, without code changes. An unsigned or ad-hoc-signed
agent falls back to the uid check (as on Linux) and reports it in `status`. Developer ID and
notarization are deferred to S7. To verify on the M2: Personal Team signatures yield a usable
designated requirement for this comparison.
