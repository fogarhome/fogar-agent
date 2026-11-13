# Real-world cross-platform Rust: patterns with links

All read from shallow clones on 2026-11-12 (read-only, nothing executed). Links pin the commit.

## Zed / GPUI (macOS, Windows, Linux, web, iOS) - zed@8e474e1
- Trait crate + one crate per OS + thin selector crate:
  `crates/gpui` defines `trait Platform`, `PlatformWindow`, `PlatformDisplay`, `ScreenCaptureSource`...;
  `crates/gpui_macos`, `gpui_windows`, `gpui_linux`, `gpui_web`, `gpui_ios`;
  `crates/gpui_platform` re-exports and provides `current_platform(headless) -> Rc<dyn Platform>` with
  a single `cfg_select!`.
  https://github.com/zed-industries/zed/blob/8e474e1598875be89a2225fb7ba0f6ebcdb7190a/crates/gpui_platform/src/gpui_platform.rs#L62
- Platform crates compile to nothing elsewhere: `#![cfg(target_os = "macos")]` at crate root and all
  OS deps under `[target.'cfg(target_os = "macos")'.dependencies]`.
  https://github.com/zed-industries/zed/blob/8e474e1598875be89a2225fb7ba0f6ebcdb7190a/crates/gpui_macos/Cargo.toml#L26
- Capability-by-default-method: `is_screen_capture_supported() -> false`, `power_mode() -> PowerMode::Unknown`,
  `request_windowing()` -> error "this platform cannot switch...", `gestures() -> None`,
  thermal state enum + `prevent_idle_sleep(reason) -> Task<Result<ActivityGuard>>` (RAII).
  https://github.com/zed-industries/zed/blob/8e474e1598875be89a2225fb7ba0f6ebcdb7190a/crates/gpui/src/platform.rs#L394
- Test double in the trait crate: `TestPlatform` (pub(crate)) behind `test-support` feature.
- Leak to learn from: `PlatformScreenCaptureFrame` is a cfg-selected per-OS type alias in the core crate.
  https://github.com/zed-industries/zed/blob/8e474e1598875be89a2225fb7ba0f6ebcdb7190a/crates/gpui/src/platform.rs#L32
- Anti-model for parallel agents: `Platform` is one very large trait.

## Espanso (text expander daemon; macOS, Windows, Linux X11/Wayland) - espanso@80d9e35
- `espanso-engine` = pure core with many tiny port traits (`TextInjector { name(); inject_text() }`,
  `funnel::Source`, `NotificationManager`, `SecureInputManager`...), deps only log/anyhow/thiserror/crossbeam.
  https://github.com/espanso/espanso/blob/80d9e353e3c0a58c77feb72671cec4a61958366f/espanso-engine/src/dispatch/executor/text_inject.rs#L27
- Platform crates (`espanso-detect`, `espanso-inject`, `espanso-ui`, `espanso-clipboard`) each = one trait +
  `mac/`, `win32/`, `x11/`, `evdev/` modules + cfg'd factory `get_source(options) -> Result<Box<dyn Source>>`.
  https://github.com/espanso/espanso/blob/80d9e353e3c0a58c77feb72671cec4a61958366f/espanso-detect/src/lib.rs#L43
- Leak to learn from: `SourceCreationOptions` carries per-OS knobs (`win32_exclude_orphan_events`, `evdev_keyboard_rmlvo`).
- Composition/glue in the binary crate: `espanso/src/cli/worker/engine/{funnel,dispatch}/...` implement engine traits over platform crates.

## cpal (audio I/O; CoreAudio, WASAPI, ASIO, ALSA, PulseAudio, PipeWire, JACK, AAudio, WebAudio) - cpal@40ff3ad (0.19.0)
- Static traits with associated types: `HostTrait { type Device; type Devices }`, `DeviceTrait { type Stream }`, `StreamTrait`.
  https://github.com/RustAudio/cpal/blob/40ff3adea81757326debba6b2befb747a6699d50/src/traits.rs#L42
- Runtime host switching via macro-generated **enum dispatch** (`impl_platform_host!`), not `dyn`.
  https://github.com/RustAudio/cpal/blob/40ff3adea81757326debba6b2befb747a6699d50/src/platform.rs#L60
- Plug-in point for custom/fake hosts: `custom::Host(Box<dyn HostErased>)` with `from_host<T: HostTrait>`.
- Errors: `Error` + `#[non_exhaustive] ErrorKind { DeviceBusy, DeviceChanged, DeviceNotAvailable, HostUnavailable,
  InvalidInput, PermissionDenied, RealtimeDenied, ResourceExhausted, StreamInvalidated, UnsupportedConfig,
  UnsupportedOperation, BackendError, Other }`.
  https://github.com/RustAudio/cpal/blob/40ff3adea81757326debba6b2befb747a6699d50/src/error.rs#L10
- Data/error callbacks run on the backend's audio thread: the model for fogar's AudioSource adapter.

## keyring v4 (credential stores) - keyring-rs@03f7683, keyring-core 1.0.0 (crate source)
- Core API crate + one crate per store, selected by target-specific optional deps in the umbrella crate.
  https://github.com/hwchen/keyring-rs/blob/03f768392c38792f378afeb27408fcb89e67a00d/Cargo.toml
  (cloned from this URL; the README links the project's wiki under github.com/open-source-cooperative/keyring-rs)
- `CredentialStoreApi::search` defaults to `NotSupportedByStore`; `persistence()` added later **with a default
  "for backward compatibility, since this API was added in a minor release"**.
- `#[non_exhaustive] enum Error { PlatformFailure(PlatformError), NoStorageAccess(PlatformError), NoEntry, ..., NotSupportedByStore(String) }`,
  `PlatformError = Box<dyn Error + Send + Sync>`.
- Conformance: "unit-test" and "leak-test" examples that store authors must run; `mock`/`sample` stores in core.

## notify (filesystem events; inotify, FSEvents, kqueue, ReadDirectoryChangesW, polling) - notify@69eac31
- `pub type RecommendedWatcher = FsEventWatcher;` etc. per cfg; portable `PollWatcher` fallback; `NullWatcher`.
  https://github.com/notify-rs/notify/blob/69eac3104220ddd365979798f0e6e1efd5ee7b53/notify/src/lib.rs#L622
- Dyn-compatibility guard test: `fn test_object_safe() { let _watcher: &dyn Watcher = &NullWatcher; }`.
  https://github.com/notify-rs/notify/blob/69eac3104220ddd365979798f0e6e1efd5ee7b53/notify/src/lib.rs#L714
- `EventHandler` implemented for closures and channels (consumer chooses delivery).
- Generic test harness over `W: Watcher` (`test::channel::<W>()`).
- Types split into `notify-types` crate (no platform deps).

## sysinfo (system probe; macOS, Linux, Windows, BSD, unknown) - sysinfo@7757e92 (0.40.3, MSRV 1.95)
- One `cfg_select!` picks `sys` module: unix/apple/linux/bsd, windows, or `unknown` fallback; CI feature
  `unknown-ci` forces the fallback to keep it compiling.
  https://github.com/GuillaumeGomez/sysinfo/blob/7757e92abf383700a4557f8bec7ad17736b6f883/src/lib.rs#L37

## Mullvad VPN app (always-on daemon + UI over IPC; macOS, Windows, Linux, Android, iOS) - mullvadvpn-app@7e1c49d
- Many small crates: `talpid-*` (platform mechanics: routing, dns, firewall, tunnel), `talpid-macos`,
  `talpid-windows`, `talpid-winfw`; `mullvad-daemon`; `mullvad-management-interface` (gRPC via tonic/prost
  over a local socket/named pipe through `tipsy`) consumed by the desktop UI and CLI.
- `talpid-time`: monotonic clock that **includes system sleep** (CLOCK_MONOTONIC on macOS, CLOCK_BOOTTIME on Linux)
  and a `sleep` that wakes at most every 60 s; `test` feature swaps to `tokio::time::Instant` for tests.
  https://github.com/mullvad/mullvadvpn-app/blob/7e1c49d41ef6cddf268bad26b4100912e97d2403/talpid-time/src/unix.rs#L10
- Leak to learn from: `#[cfg(unix)]` variants in the management-interface `Error` enum.
  https://github.com/mullvad/mullvadvpn-app/blob/7e1c49d41ef6cddf268bad26b4100912e97d2403/mullvad-management-interface/src/lib.rs#L56
- Platform modules inside `talpid-core` selected with `cfg_if!` (`firewall/{macos,linux,windows,android}.rs`).

## scap / xcap (screen capture) - scap@c03f15a (Aug 2025), xcap@6b4ad60
- scap: per-OS `utils::{is_supported, has_permission, request_permission}` - permission is part of the API surface.

## rust-analyzer (architecture docs + tidy tests) - rust-analyzer@8cbebb0
- `docs/book/src/contributing/architecture.md`: explicit "**Architecture Invariant:**" paragraphs
  (e.g. "`syntax` crate is completely independent from the rest of rust-analyzer. It knows nothing about salsa or LSP.").
- `xtask/src/tidy.rs` runs as a `#[test]` and checks Cargo.toml conventions (e.g. internal deps must/
  must not carry versions) - precedent for archtest-as-cargo-test.
  https://github.com/rust-lang/rust-analyzer/blob/8cbebb0c2c23562b11e4d56efb85ca457c758b2f/xtask/src/tidy.rs#L83

## cargo-deny - cargo-deny@0fabb18 (0.20.2)
- `wrappers`, `allow-workspace`, `use-instead`, `[graph] targets` documented in `docs/src/checks/bans/cfg.md`
  and `docs/src/checks/cfg.md`; wrapper check over direct dependents in `src/bans.rs`.
