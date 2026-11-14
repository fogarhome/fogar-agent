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
- macOS adapter is a stub (`accept` never resolves): no UI IPC on macOS until S4.

## To verify on the M2
- `LOCAL_PEERTOKEN` yields the peer's audit token for a client connecting from the signed app.
- `SecCodeCheckValidity` passes for Developer ID and dev builds with the intended requirement.
- Socket path stays < 104 bytes for real home directories.
