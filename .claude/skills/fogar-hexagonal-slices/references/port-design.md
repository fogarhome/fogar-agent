# Port design in Rust (fogar-agent)

Labels: [primary] = rustc/cargo behaviour observed here, official docs, or project source;
[secondary] = reputable write-ups; [unverified] = reasoned, not checked. Toolchain used for
experiments: rustc 1.97.0 on Linux. Current stable on 2026-11-12: 1.99.x is the latest
announced release on blog.rust-lang.org (1.99.0 on 2026-10-01, 1.99.1 on 2026-10-15); 1.100 is
due on the 6-week cadence but no announcement was found [primary: blog index].

## 1. Dispatch: generics vs `dyn` vs enum

| Situation | Choose | Why |
|---|---|---|
| Port used in a loop many times/sec (audio buffers, frames) inside core | **Generic param** `E<P: Port>` | Static dispatch, inlining, no boxing of futures. Cost: monomorphisation (compile time/binary size), type params leak into engine structs. |
| Port chosen once at startup, called <= a few times/sec (probe, secrets, autostart, IPC accept) | **`Box<dyn Port>` / `Arc<dyn Port>`** | Indirect call ~1-2 ns measured [primary: microbench below]; keeps engine types simple; allows runtime choice (fallback adapter). Port must stay dyn-compatible. |
| Closed set of backends known at compile time, runtime switch needed | **enum wrapper** (cpal `impl_platform_host!`) | No vtable, exhaustive match; cost: macro/boilerplate. [primary: cpal src/platform.rs:60] |
| Async port with runtime selection | **generic** in core; if you truly need `dyn`, a boxed twin trait (hand-written, `dynosaur`, or `async-trait`) | Native `async fn`/RPITIT traits are not dyn-compatible on stable [primary: E0038 below]. |

Microbenchmark (this sandbox, `-C opt-level=3`, 20M calls, trivial body) [primary]:
static async ~0.6 ns/call; boxed `dyn` async ~13-15 ns/call (one heap alloc per call);
`dyn` sync ~1.5-1.6 ns/call. Conclusion: the dispatch mechanism is irrelevant at the
call rates of a daemon *if you never cross a port per sample/pixel*. Cross ports per
**buffer/frame/batch**, never per element.

Rule of thumb for fogar: **sync, dyn-compatible ports by default; async + generic only
where the operation is real I/O with latency (uplink, IPC, secrets on some OSes).**
Guard dyn-compatibility at compile time, as notify does with `test_object_safe`
[primary: notify/src/lib.rs:714]:

```rust
const _: Option<&dyn SystemProbe> = None; // breaks the build if the trait stops being dyn-compatible
```

## 2. Async in traits (status 2026-11)

- `async fn` and return-position `impl Trait` in traits are stable since **Rust 1.75**
  (Dec 2023) [secondary: Rust blog 2023-12-21 "Announcing async fn and return-position impl Trait in traits"; confirmed by compiler behaviour].
- rustc warns on `async fn` in a **public** trait [primary, rustc 1.97]:
  `warning: use of async fn in public traits is discouraged as auto trait bounds cannot be specified` (`#[warn(async_fn_in_trait)]`),
  and suggests `fn send(&self) -> impl Future<Output = ..> + Send`.
- Generic code that needs `Send` futures fails with `async fn` traits [primary, E0277]:
  `impl Future<Output = ..> cannot be sent between threads safely`. Fix: desugar in the trait
  with `+ Send`; implementors may still write `async fn` in the impl [primary: compiled].
- A trait with `async fn` is **not dyn compatible** [primary, E0038: "...because method `send` is `async`"].
- **Return Type Notation** (`T::method(..): Send`) is still nightly-only (`return_type_notation`);
  its 2026 project goal says stabilization is blocked on the next-gen trait solver and planned
  "late this year", together with TAIT; an earlier stabilization PR (rust-lang/rust#138424) was closed
  [primary: goals.rust-lang.org/2026/rtn.html]. Do not design around RTN.
- Native `dyn` async traits: still not stable; there is a 2026 project goal "Box notation for dyn
  async trait" (page not readable from here) [unverified detail].
- Workarounds (current crates.io versions, 2026-11-12 sparse index) [primary]:
  `trait-variant 0.1.3` (`#[trait_variant::make(Name: Send)]` generates a Send variant),
  `dynosaur 0.3.1` (generates `DynTrait` that boxes only on dynamic dispatch),
  `async-trait 0.1.92` (boxes every call, static or dynamic).
- Recommended for fogar: write the desugared form by hand (`fn x(&self) -> impl Future<Output=T> + Send`),
  require `Send + Sync + 'static` on the port trait, and add a blanket-implemented boxed twin
  (`DynUplink`) only if/when a runtime choice is needed. See `example-workspace/crates/fogar-ports/src/uplink.rs`.

## 3. Cancellation safety (part of the contract)

Any `.await` can be the last: `select!`, timeouts and shutdown drop futures. Rules:
- Port docs must say for each async method whether it is **cancel-safe** (dropping it loses
  nothing and leaves the adapter usable). Tokio documents this per method, e.g. `mpsc::Receiver::recv`
  "This method is cancel safe" [primary: tokio-1.54.1 src/sync/mpsc/bounded.rs].
- Core: mutate state only **after** an await completes (see `Governor::tick`), so dropping
  `run()` at any await is harmless.
- Adapter: never hold a half-written frame across an await without a recovery path; if a write
  can be torn, the next call must return `Disconnected`/reconnect.
- Contract suite checks it: create a future, drop it unpolled/partially polled, call again
  (`survives_cancelled_send_async`).

## 4. Associated types vs concrete types

- Use **associated types** when each adapter has its own natural handle type and the core
  never stores heterogeneous adapters (cpal `HostTrait::Device`, `DeviceTrait::Stream`
  [primary: cpal src/traits.rs:42,129]). Downside: `dyn Trait<Assoc = X>` must name the type; harder to box.
- Use **concrete port-owned types** (`SystemSnapshot`, `EventStream<T>`) for anything the core
  inspects. Zed shows the cost of not doing this: `ScreenCaptureFrame(pub PlatformScreenCaptureFrame)`
  is a per-OS type alias selected with `cfg` *inside the core crate* (CVImageBuffer on macOS, scap
  frame elsewhere) [primary: zed crates/gpui/src/platform.rs:32-37] - a leak we avoid by
  making frames/pixels a port-owned type (or an opaque handle + `to_rgba(&mut Vec<u8>)` method).

## 5. Errors at ports (thiserror)

Pattern (keyring-core, cpal, notify all converge on it) [primary]:
- One `#[non_exhaustive]` error enum **per port**, categories the core can act on:
  `Unsupported`, `PermissionDenied`, `Transient` (retryable), `Platform(Box<dyn Error + Send + Sync>)`.
  keyring-core: `PlatformError = Box<dyn std::error::Error + Send + Sync>`, `NoStorageAccess`,
  `NotSupportedByStore(String)` [primary: keyring-core 1.0.0 src/error.rs]. cpal 0.19: `Error` struct +
  `#[non_exhaustive] ErrorKind { DeviceBusy, DeviceNotAvailable, HostUnavailable, PermissionDenied,
  UnsupportedConfig, UnsupportedOperation, BackendError, ... }` [primary: cpal src/error.rs].
- Never put `#[cfg(...)]` on error **variants** in a shared crate: consumers' matches then differ per
  OS. Mullvad's management interface does this (`#[cfg(unix)] NoGidError`, ...) [primary:
  mullvad-management-interface/src/lib.rs:56] - fine for a leaf crate, wrong for a port.
- Keep OS error detail as a `#[source]`, for logs only. Core matches on the category.
- `thiserror 2.0.21` is `#![no_std]` with a `std` feature [primary: crate source], so the same
  style works in `fogar-proto` with `default-features = false`.
- Add `is_retryable()` style helpers on the error so policy (backoff) lives in core.

## 6. Event streams and backpressure

- Shape: adapter owns a **bounded** channel sender; core gets a receiver newtype
  (`EventStream<T>`). Newtype so the channel implementation can change.
- `tokio::sync` is runtime-agnostic ("All synchronization primitives provided in this module are
  runtime agnostic", except `_timeout` methods) [primary: tokio src/sync/mod.rs:440], so ports can
  depend on `tokio` with only `features = ["sync"]` without choosing a runtime.
- OS callbacks (CoreAudio IO proc, NSNotification, WinEvent hooks) must **never block**: use
  `try_send`; on `Full` drop + count (`dropped()`), on `Closed` stop producing. The drop policy is
  part of the port contract and covered by the contract suite.
- Per-stream policy table:

| Stream | Policy when full | Why |
|---|---|---|
| Audio buffers | drop **oldest** (ring buffer) or newest; count drops | realtime thread must not block; stale audio is worthless |
| Screen frames | keep **latest only** (`tokio::sync::watch` or 1-slot) | consumers want the newest frame |
| System/power events | small bounded queue, drop+count, core resyncs with `snapshot()` | events are hints; state is re-readable |
| IPC requests from UI | bounded, `send().await` (backpressure to client) | correctness over latency |

- Pull alternative: for low-rate data prefer `snapshot()` polling with an adaptive interval over
  a push stream; fewer threads, fewer wakeups (energy).
- Notify's `EventHandler` trait is implemented for closures and channels, letting the caller pick
  the delivery mechanism [primary: notify/src/lib.rs:386]. Use this when the consumer is not the core.

## 7. Capability negotiation ("not supported" gracefully)

Three proven mechanisms, combine them:
1. **Capability struct queried once** (`ProbeCaps` with `Support::{Native, Emulated, Unsupported}`),
   stable for the adapter's lifetime, checked by the contract suite against actual behaviour.
   Construct a `#[non_exhaustive]` struct outside its crate by copying a const and setting fields
   (`let mut c = ProbeCaps::NONE; c.load = Support::Native;`) [primary: compiled].
2. **Defaulted trait methods that return "unsupported"**: zed `is_screen_capture_supported() -> false`,
   `power_mode() -> PowerMode::Unknown`, `request_windowing()` returns an error by default
   [primary: zed platform.rs:409,434,552]; keyring-core `search()` defaults to `NotSupportedByStore`,
   and `persistence()` got a default "since this API was added in a minor release" [primary: api.rs:220,235].
3. **`Option` fields in snapshots** for values that may be absent at runtime (no battery).

Plus **permission state** as a first-class capability for TCC-gated features (screen recording,
microphone): scap exposes `is_supported() / has_permission() / request_permission()` per OS
[primary: scap src/utils/mod.rs]. Model it as `PermissionState::{Granted, Denied, NotDetermined, NotApplicable}`.

Fallback adapters: notify picks `RecommendedWatcher` per OS but ships a portable `PollWatcher`
and a `NullWatcher` [primary: notify lib.rs:322,622]; sysinfo has an `unknown` module that compiles
for any target and a CI feature `unknown-ci` to keep it compiling [primary: sysinfo src/lib.rs:37].
For fogar: every port gets a `Null*`/`Unsupported*` adapter usable on any OS.

## 8. Evolving ports without breaking adapters

Cargo SemVer reference [primary: doc.rust-lang.org/stable/cargo/reference/semver.html]:
- *Major*: adding a non-defaulted trait item; adding a trait item that makes the trait non-object-safe;
  adding an enum variant without `#[non_exhaustive]`; adding a public field when no private field exists;
  removing a Cargo feature.
- *Possibly-breaking*: adding a **defaulted** trait item (name ambiguity); changing MSRV.
- *Minor*: adding a Cargo feature.

Inside one workspace there is no published semver, but the same rules decide **how many
worktrees break**. Procedure:
1. New capability -> new **defaulted** method returning `Unsupported` + new `Support` field
   defaulting to `Unsupported` in `NONE` + contract clause gated on the capability. Zero adapters break.
2. Changed semantics -> add `v2` method with default delegating to old one; migrate adapters;
   remove old one in a later slice.
3. Breaking signature -> port PR changes trait **and** fake **and** contract suite **and** every
   adapter in one PR (owner: port owner), or split the port.
4. API guidelines that keep this cheap [primary: rust-lang api-guidelines checklist]: C-OBJECT
   (object-safe if useful as trait object), C-STRUCT-PRIVATE / `#[non_exhaustive]`, C-NEWTYPE-HIDE
   (e.g. `EventStream`), C-GOOD-ERR, C-SEND-SYNC, C-DEBUG. C-SEALED is for traits *outsiders must not
   implement* - not for ports, which exist to be implemented.
5. If `fogar-ports` is ever published or shared with a separate UI repo, run `cargo-semver-checks`
   in CI [secondary].

## 9. Keep ports small and honest

- <= ~5 methods, one reason to change, named after the **core's need** ("SystemProbe",
  "WorkScheduler"), not the OS API ("IOKitPowerSources").
- Zed's `Platform` trait (~100 methods: windows, menus, clipboard, thermal, sleep, screen capture...)
  works for an app framework but is the anti-model for parallel agents: every change touches every
  adapter. Split by capability.
- Prefer value types in/out (snapshots, frames as `Vec<u8>`/`bytes`), no callbacks into core.
- **Lowest-common-denominator trap**: do not shrink a port to what all OSes share. Model the
  richest platform (macOS first) as optional capabilities (thermal state mirrors
  `ProcessInfo.ThermalState`; power mode; low-power mode), and let others report `Unsupported`/
  `Emulated`. Zed does exactly this with `ThermalState {Nominal, Fair, Serious, Critical}` and
  `PowerMode::Unknown` [primary: zed platform.rs:546-556,759].
- **Leaky-options trap**: espanso's `SourceCreationOptions` carries `win32_exclude_orphan_events`,
  `evdev_keyboard_rmlvo`... [primary: espanso-detect/src/lib.rs:65]. Platform knobs belong in the
  adapter's own config struct, built by the composition root, never in the port.

## 10. Sketches for fogar's planned ports

| Port | Sync/async | Dispatch | Notes |
|---|---|---|---|
| `AudioSource` | callback thread -> `EventStream<AudioBuf>` | dyn | adapter owns realtime thread; fixed-size buffers from a pool; permission state; caps: sample rates, channels |
| `ScreenCapture` | async start/stop + latest-frame stream | dyn | permission state (TCC); frame as port-owned type or opaque handle; caps: per-window, cursor, HDR |
| `SystemProbe` | sync snapshot + optional events | dyn | worked example in `example-workspace` |
| `UiIpc` | async (accept/recv/send) | generic | transport (UDS / named pipe) is adapter detail; message schema lives in `fogar-proto` or a sibling `fogar-ipc-proto` (mullvad: gRPC over local socket/pipe via `tipsy` [primary]) |
| `Lifecycle` (autostart, single instance, signals) | sync | dyn | launchd plist / LaunchAgent vs Windows service vs systemd unit: capability flags (`can_autostart`, `is_supervised`) |
| `PowerPolicy` (QoS / priority / prevent sleep) | sync, RAII guard | dyn | `prevent_idle_sleep(reason) -> Guard` like zed [primary: platform.rs:548]; `set_qos(Background/Utility)` maps to macOS QoS classes, Windows EcoQoS, Linux nice/ionice [unverified mapping] |
| `SecretStore` | sync (async optional) | dyn | mirror keyring-core: `NoEntry`, `NoStorageAccess`, `NotSupportedByStore`, persistence capability [primary] |
| `Clock` | `now()` sync, `sleep()` async | generic | define whether time includes system sleep: mullvad uses CLOCK_MONOTONIC on macOS / CLOCK_BOOTTIME on Linux and caps each tokio sleep at 60 s so long waits survive suspend [primary: talpid-time]; std docs: `Instant::now` uses `clock_gettime(CLOCK_UPTIME_RAW)` on Darwin, `CLOCK_MONOTONIC` on UNIX, `QueryPerformanceCounter` on Windows, and "it is also not specified whether system suspends count as elapsed time or not" [primary: doc.rust-lang.org/std/time/struct.Instant.html]. So the Clock port must state its suspend semantics and the contract/soak tests must cover sleep/wake |
