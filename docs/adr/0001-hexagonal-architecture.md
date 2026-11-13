# ADR-0001: Hexagonal architecture (ports & adapters) delivered in vertical slices

Status: **PROVISIONAL** (accepted for slice 0; re-confirm after S2, the first real macOS adapter)

## Decision
- Inner crates: `fogar-proto` (wire, no_std), `fogar-ports` (traits + value types), `fogar-core`
  (engine). Outer: `fogar-host` (portable adapters), `fogar-agent-<os>` (OS adapters),
  `fogar-agentd` (composition root). Test support: `fogar-testkit`, `fogar-testalloc` (dev-only).
- Rules R1–R8 are mechanical (`crates/fogar-archtest`, `deny.toml`, per-crate `clippy.toml`).
- Ports are small, sync + dyn-compatible by default; async ports use `fn -> impl Future + Send`
  and the core is generic over them (`GatewayLink`, `IpcServer`, `Clock`).
- Every source of nondeterminism is a port: time (`Clock`), randomness (`Random`), network,
  secrets, audio, IPC, system probe, power policy.

## Deviations from the skill (fogar-hexagonal-slices), recorded here
- `fogar-host` exists although the slice-0 brief did not list it: the WebSocket link and tokio
  clock are portable and needed by macOS and Linux alike (skill §1 places them there).
- `fogar-core` depends on tokio (`sync`, `rt`, `macros`) and tokio-util (`rt`) for structured
  concurrency (TaskTracker, CancellationToken, select!). The skill's example core had no runtime;
  the brief requires supervision inside core. tokio time/net/fs/signal remain banned in core by
  clippy.toml, and archtest R4 proves no OS/I-O crate is reachable from core.
- `fogar-testalloc` added so the counting allocator's `unsafe` stays out of fogar-core.
- `fogar-agentd` (composition root) may have target-specific deps (archtest R3 exception).

## To verify on the M2
- The macOS adapter crate compiles and passes the unchanged contract suites on macOS CI.
- No port had to change shape to accommodate the real macOS adapters (if one did, record why).
