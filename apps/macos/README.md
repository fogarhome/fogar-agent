# Fogar Desktop (macOS)

The companion app for `fogar-agent`: a SwiftUI app with a main window (Conversación, Estado, Iniciar sesión, Ajustes),
Settings (⌘,) and a menu bar quick view. It talks to the agent over the agent's Unix socket
(ADR-0002) using the wire types generated from `crates/fogar-host/src/ipc_wire.rs` (ADR-0004).
macOS 14+. Bundle id `com.fogar.desktop` (GUESSED: the agent's peer check requires exactly this).

## Build and run

```sh
scripts/build-app.sh            # build/Fogar Desktop.app, release, signed
open "build/Fogar Desktop.app"
```

Signing uses the first "Apple Development" identity in the keychain (free Personal Team), with the
hardened runtime the agent's "same signer" check needs. Without one the app is signed ad hoc and the
agent treats it as `uid_only`: status works, login/logout are refused.

## The agent inside the app (S2 PR 6)

The release build carries the Rust agent:

```
Fogar Desktop.app/Contents/
  MacOS/FogarDesktop                         the app
  MacOS/fogar-agentd                         the agent (Info.plist embedded in __TEXT,__info_plist)
  Library/LaunchAgents/com.fogar.agent.plist LaunchAgent, BundleProgram = Contents/MacOS/fogar-agentd
```

`scripts/build-app.sh` builds the agent (`cargo rustc --release -p fogar-agentd`), signs it first
(identifier `com.fogar.agent`, hardened runtime, `Packaging/Agent/fogar-agentd.entitlements`: only
the microphone, never `get-task-allow`), then the app (`Packaging/FogarDesktop.entitlements`:
the microphone too, because macOS attributes the agent's microphone to the app), same identity,
inside out, no `--deep`; then it checks what it signed and fails on any mismatch.

**Runbook (on the Mac)**

- Install: build, copy `build/Fogar Desktop.app` to `/Applications`, open it. The first launch
  registers the agent (`SMAppService.agent(plistName: "com.fogar.agent.plist")`); launchd starts it
  at once and at every login. Ajustes → Agente shows the state: active, waiting for approval
  (button to System Settings → General → Login Items), or not registered (button to register).
- State from Terminal: `launchctl print gui/$(id -u)/com.fogar.agent` (state, pid, last exit) and
  `sfltool dumpbtm | grep -B2 -A8 com.fogar.agent` (the login item record).
- Remove the registration: Ajustes → Agente → «Quitar el registro» (launchd stops it and will not
  start it again); or switch it off in Login Items. The app never re-registers by itself after the
  first launch.
- From the S1 hand-run agent to the bundled one: stop the hand-run agent first (Ctrl-C, or
  `launchctl bootout gui/$(id -u)/<label>` if you loaded a plist of your own, then delete that
  plist); otherwise the bundled agent finds the socket in use and exits. The state directory
  (`~/Library/Application Support/com.fogar.agent`) and the Keychain service (`com.fogar.agent`)
  are the same. The session is ONE item, `gateway_account`; a 4.x `gateway_pairing` item is never
  read and is deleted at the next sign-in. **GUESSED:** that the bundled agent reads the item
  without a dialog (it may have been created by another binary); if macOS asks, «Permitir
  siempre» once, or sign out and in again from the app.
- After rebuilding: macOS keeps the microphone permission while the signer stays the same (spike,
  step 0). If the plist changes, remove the registration and register again.
- Agents started by hand with `--state-dir` other than the default use their own Keychain service
  (`com.fogar.agent.dir-<hash of the path>`), so measurement runs never touch the real session.

`build-app.sh` is **for development only**: Apple Development identity (Personal Team), no
secure timestamp (`--timestamp=none`), no notarization. A build to give to anyone else needs
Developer ID, timestamp, notarization and updates: S8. Out of scope here: all of that and an
installer.

## Without the Rust agent: the stand-in

`fogar-standin` speaks the same frames and plays scripted states (synthetic data). It listens on
`/tmp/fogar-standin/agent.sock` by default and refuses the real agent's socket path.

```sh
swift run fogar-standin --phase ready --gateways 2
scripts/build-app.sh --debug
FOGAR_AGENT_SOCKET=/tmp/fogar-standin/agent.sock FOGAR_AGENT_UNSIGNED=1 \
  "build/Fogar Desktop (debug).app/Contents/MacOS/FogarDesktop"
```

The app authenticates the server behind the socket before sending anything (ADR-0002, mirrored).
Always: the socket and its directory are the user's (directory 0700, no symlinks) and the server
runs as the same user. That is enough for the read-only status (`hello`, `status`, `subscribe`,
`discover`), so an agent not yet packaged and signed still shows its state. The conversation and
`login` (it carries the password), `logout`, `reconnect` also need the agent's code signed by the app's own Team with the
identifier `com.fogar.agent` (GUESSED until PR 6 packages the agent), the hardened runtime, no
`get-task-allow` and no debugger attached. The unsigned stand-in passes those only in a debug build
started with `FOGAR_AGENT_UNSIGNED=1`; a release build has no such switch (CI checks the release
binary does not even contain the name).

Only debug builds honour `FOGAR_AGENT_SOCKET`. Phases: `not_paired`, `connecting`, `ready`,
`reconnecting`, `parked_repair`, `parked_agent_refused`, `parked_missing_scope`, `parked_upgrade_client`, `parked_client_error`, `stopping` (also typed
on the stand-in's stdin to change the live state). `--version 6` plays a newer agent, `--refuse-login`
an agent that refuses this app, `--boot safe` the crash-loop safe mode, `--peer uid_only` an unverified
app (login/logout refused, like the core), `--old` a pre-S1 core (no live status),
`--logout-offline` a logout the gateway did not hear.

Sign in as `sito` / `fogar` (fixed test credentials, synthetic). Other passwords script the
failures: `inseguro` insecure_transport, `muchos` too_many_attempts, `lejos` unreachable, `otro`
not_a_gateway, `roto` gateway_error, `llavero` storage_failed, `ocupado` busy, `mal` bad_address;
anything else invalid_credentials. Like the agent, an address that is not `https://`, a `.ts.net`
name, a `100.x` tailnet address or this Mac is needs_secure_address (the password would cross the
home network in clear text), and signing in while signed in is already_signed_in.

The sign-in screen keeps the password only in its own state and empties it after every attempt
and when it goes away. The app never prints or logs a `WireRequest`: `login` carries the password
and the generated `WireLogin` has no redaction.

Conversations (IPC v6, only with `--peer same_signer`, the default): two example conversations in
the list; «Nueva» leaves nothing open and the next text creates one. Typed text gets a scripted
reply (preparing → thinking → «Usando ha_call_service…» → text); the conversation shows «en marcha»
and goes to the top meanwhile. A text sent while a reply runs waits («Pendiente») and is answered
next; «Cancelar» withdraws it (it disappears). Text with «error» or «lento» ends in an error or a
timeout. Nothing plays, and dictation is off until PR 5 (the microphone says «sin transcriptor»);
`--transcriber up` brings the scripted take back to work on its look: partials of a phrase, then
5 s later the agent cuts it to review. `--mic not_determined|granted|denied|restricted` (default
`not_determined`), `--mic-deny` (the permission prompt says no), `--mic-prompt-secs N` (how long the
prompt takes), `--no-mic`, `--transcriber up|down`, `--tts up|down` (status only).
On stdin: `push` (Fogar writes on its own), `otro <texto>` (another device, «Cocina», writes in the
open conversation), `solo_lectura` / `escritura` (the account loses / gets `conversations:write`),
`mic <state>`, `transcriber <state>`, `tts <state>`, `silencio` / `seguridad` (cut the open take),
`frase_perdida`. Launch arguments `-seccion iniciarSesion` and `-apariencia clara|oscura|sistema` pick the
screen and appearance.

## Wire types

`Sources/FogarWire/Wire.swift` is generated; never edit it.

```sh
scripts/gen-wire.sh          # regenerate (typeshare-cli 1.13.4)
scripts/gen-wire.sh --check  # fails if the committed file is stale
```

## Tests

```sh
swift test
```

Golden JSON from the Rust tests, framing, the app's links to the agent
(status and conversation) against a scripted agent on a real Unix socket, the conversation model
(take gestures, visible-turn rule, microphone states), the agent's registration (`ServicioAgente`
against a fake registry), and the stand-in's conversation rules. In CI (`macos` job),
`scripts/ci-signed-agents.sh` first signs a small test server six ways with a temporary
self-signed certificate, and `SignedAgentTests` runs the app's real peer check against each:
`com.fogar.agent` with the hardened runtime is accepted; another identifier, no hardened runtime,
`get-task-allow`, `allow-dyld-environment-variables` or `disable-library-validation` are refused (the certificate's hash stands in for Apple's anchor, and the real
anchor is checked to refuse it). Without those variables that suite is skipped.
