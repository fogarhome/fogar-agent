# ADR-0004: No UniFFI; separate processes share a wire contract, not code

Status: **PROVISIONAL**

## Decision
The Swift UI and the Rust agent are separate processes talking over the IPC socket (ADR-0002).
They share the message contract, not a linked library: no UniFFI, no Rust staticlib in the app.
Swift `Codable` types will be generated with typeshare from the IPC wire types (S1), with
hand-written Swift framing (4-byte BE length + JSON). Revisit UniFFI only if the Swift client
grows past ~500 lines of protocol code (fogar-macos-daemon §1).

## Slice 0 state
Wire types exist only in `fogar-agent-linux` (`WireRequest`/`WireResponse`); S1 moves them to a
shared, typeshare-annotated module.

## To verify on the M2
- typeshare output compiles in the Swift app target and round-trips against the agent.
- Version handshake (`hello{version}`) rejects mismatched UI/agent pairs after a Sparkle update.
