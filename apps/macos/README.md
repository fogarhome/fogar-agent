# Fogar Desktop (macOS)

The companion app for `fogar-agent`: a SwiftUI app with a main window (Estado, Emparejar, Ajustes),
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
agent treats it as `uid_only`: status works, pair/unpair are refused.

## Without the Rust agent: the stand-in

`fogar-standin` speaks the same frames and plays scripted states (synthetic data):

```sh
swift run fogar-standin --socket /tmp/fogar-standin/agent.sock --phase not_paired --gateways 2
scripts/build-app.sh --debug
FOGAR_AGENT_SOCKET=/tmp/fogar-standin/agent.sock "build/Fogar Desktop.app/Contents/MacOS/FogarDesktop"
```

Only debug builds honour `FOGAR_AGENT_SOCKET`. Phases: `not_paired`, `connecting`, `ready`,
`reconnecting`, `parked_repair`, `parked_agent_refused`, `parked_client_error`, `stopping` (also typed
on the stand-in's stdin to change the live state). `--version 3` plays a newer agent, `--refuse-pair`
an agent that refuses this app, `--boot safe` the crash-loop safe mode, `--peer uid_only` an unverified
app. Pairing codes decide the outcome: `482913` pairs; `111111` invalid_code, `222222` unreachable,
`333333` not_a_gateway, `444444` gateway_error, `555555` storage_failed, `666666` busy, `777777`
bad_address. Launch arguments `-seccion emparejar` and `-apariencia clara|oscura|sistema` pick the
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

Golden JSON from the Rust tests, framing, the pairing code check, and the app's link to the agent
against a scripted agent on a real Unix socket.
