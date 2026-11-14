# Swift interop and safe upgrade/self-update

## A. Swift ↔ Rust: what crosses the boundary

The SwiftUI app and the Rust agent are **separate processes**. Only the wire protocol is shared, not memory. So the question is how Swift gets correct types and framing for `fogar-protocol`.

| Option | What it is | Verdict |
|---|---|---|
| **Generate Swift `Codable` types from the Rust protocol crate (typeshare)** | `typeshare` 1.0.5 (annotations) + `typeshare-cli` 1.13.4 generate Swift, Kotlin, TS, Go, Python and Scala [primary: typeshare-core 1.13.4 `src/language/`]. Data-carrying enums **must** be adjacently tagged `#[serde(tag = "type", content = "content")]`; typeshare rejects other forms [primary: parser.rs `SerdeTagRequired`/`SerdeContentRequired`] | **Recommended for v1.** Types come from one source of truth, JSON on the wire, and about 150 lines of hand-written Swift framing (length-prefixed frames over `NWConnection`/`FileHandle` on the UDS) |
| UniFFI 0.32.2 (`fogar-client` Rust lib embedded in the Swift app) | Swift calls a Rust client library in-process. That library does framing, handshake and reconnection | **Later, if the client logic grows** (reconnect/backoff, streaming, versioning). Cost: an xcframework build, a Rust staticlib signed inside the UI app, and two Rust toolchains in the Xcode build |
| swift-bridge 0.1.59 | Rust↔Swift FFI codegen | No advantage over UniFFI for this use |
| JSON Schema (schemars 1.2.3) → quicktype → Swift | Language-neutral schema file | Fine alternative if Windows/Linux UIs (C#, GTK) also need types. Commit the schema as a contract artifact |
| Protobuf (prost 0.14.6 + swift-protobuf) | IDL-first | Heavier, binary-only debugging. Only if the protocol must be consumed by many languages |

**Plain recommendation:** `fogar-protocol` (pure serde types, `#[typeshare]`, adjacently tagged enums, explicit `protocol_version`) → generate `FogarProtocol.swift` in CI → check in → CI fails if the generated file differs. Hand-write the Swift transport. Revisit UniFFI only if the Swift client exceeds about 500 lines. Also emit a JSON Schema (schemars) as a contract test artifact so non-Swift clients can follow.

Rules:
- Every message type carries the version, or is reached only after `Hello`. Additive changes are minor. Removals or renames require a major bump and a deprecation window of one release.
- No `u64` that Swift `JSONDecoder` cannot hold [unverified edge]. No `HashMap` with non-string keys. Timestamps as RFC 3339 strings or `i64` ms.
- Golden-file tests: Rust serialises fixtures, Swift decodes them in the UI test target, and the reverse.

## B. Upgrade / self-update (macOS)

**Unit of update = the whole `Fogar.app` bundle via Sparkle. The daemon never replaces its own binary on macOS.** (`self-replace` 1.5.1 / `self_update` 1.3.3 exist but are for standalone CLIs, and they would break the signature/notarization chain and the SMAppService bundle model.)

Facts:
- After a bundle replacement, the old process keeps running the old code from a now-invalid bundle. Kevin Elliott (DTS): the system does not relaunch helpers. Recommendation: "If your app is an agent and you've configured your plist correctly, you should be able to simply exit() and let launchd open your new version again" [secondary: forum 763268].
- Updates are an atomic swap of the bundle directory [secondary]. The helper "can't find things… in its bundle directory because its entire bundle structure is no longer valid" → **load everything the agent needs at startup. Never lazily read resources from the bundle.**
- Changing the helper's signing identifier between versions caused launch-constraint failures [secondary: 795022] → identifier frozen forever.
- There is no documented way to make launchd re-read a changed SMAppService plist except unregister → register [secondary: 795022 "Not that I'm aware of"; 768592 race].

Procedure:
1. Sparkle installs the new `Fogar.app` (the user is told, and the UI app relaunches).
2. The new UI app starts and connects. `Hello{app_version}` arrives at the old agent. Then:
   - If `agent_version < app_version`, the agent finishes or pauses work, persists state, and **exits with a dedicated non-zero code** (e.g. 75 = EX_TEMPFAIL), so `KeepAlive{SuccessfulExit=false}` relaunches it from the new bundle after `ThrottleInterval`. The UI shows "updating agent" and retries the connection with backoff.
   - A newer agent with an older UI is refused (major) or tolerated (minor).
3. If the **plist content** changed between versions (keys, not binary), the UI does `unregister(completionHandler:)` → wait → `register()` with retry/backoff, once per version (store `registered_plist_hash`).
4. State/schema migrations live in core and run at agent startup. They must be idempotent and versioned, with a backup file. The agent never downgrades the schema; if the state is newer than the code, go to safe mode.
5. Uninstall case (the user deleted the app): exit only when the agent's own executable path no longer exists. Check `std::env::current_exe()` metadata on wake and at most hourly. Never decide this from failed IPC handshakes. Use exit(0) so launchd does not respawn the agent [unverified: how launchd/BTM behave after the bundle is deleted. Test it].

Windows/Linux:
- Linux: the package manager replaces the binary. Then `systemctl --user restart fogar-agent` from the postinst (per user, tricky) or the same version-handshake exit (with `Restart=on-failure` and a non-zero exit code). AppImage: an external updater. Cost: low–medium.
- Windows: the installer (MSIX/MSI) stops the logon task or process, replaces files (a running exe cannot be overwritten), and restarts. The handshake logic is the same. Cost: medium (file locking).
- The **core logic** (handshake, exit-for-update decision, migrations, safe mode) is shared. Only "who replaces files" is platform-specific.
