# Fogar Desktop (macOS)

The companion app for `fogar-agent`: a SwiftUI app with a main window (Conversación, Estado, Emparejar, Ajustes),
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

`fogar-standin` speaks the same frames and plays scripted states (synthetic data). It listens on
`/tmp/fogar-standin/agent.sock` by default and refuses the real agent's socket path.

```sh
swift run fogar-standin --phase ready --gateways 2
scripts/build-app.sh --debug
FOGAR_AGENT_SOCKET=/tmp/fogar-standin/agent.sock FOGAR_AGENT_UNSIGNED=1 \
  "build/Fogar Desktop.app/Contents/MacOS/FogarDesktop"
```

The app authenticates the server behind the socket before sending anything (ADR-0002, mirrored):
the socket and its directory must be the user's (directory 0700, no symlinks), the server must run
as the same user, and its code must be signed by the app's own Team with the identifier
`com.fogar.agent` (GUESSED until PR 6 packages the agent) and the hardened runtime. The unsigned
stand-in passes only in a debug build started with `FOGAR_AGENT_UNSIGNED=1`; a release build has no
such switch.

Only debug builds honour `FOGAR_AGENT_SOCKET`. Phases: `not_paired`, `connecting`, `ready`,
`reconnecting`, `parked_repair`, `parked_agent_refused`, `parked_client_error`, `stopping` (also typed
on the stand-in's stdin to change the live state). `--version 3` plays a newer agent, `--refuse-pair`
an agent that refuses this app, `--boot safe` the crash-loop safe mode, `--peer uid_only` an unverified
app (pair/unpair refused, like the core), `--old` today's pre-S1 core (no live status). Pairing codes decide the outcome: `482913` pairs; `111111` invalid_code, `222222` unreachable,
`333333` not_a_gateway, `444444` gateway_error, `555555` storage_failed, `666666` busy, `777777`
bad_address.

Conversation (IPC v4, only with `--peer same_signer`, the default): typed text gets a scripted
reply (preparing → thinking → «Usando ha_call_service…» → text); text with «error» or «lento» ends in
an error or a timeout. The reply's audio «plays» only after the app's `turn_visible` and is dropped
2 s later otherwise (the turn shows «sin sonido»). A take plays partials of a phrase, then goes quiet:
5 s later the agent cuts it to review. `--mic not_determined|granted|denied|restricted` (default
`not_determined`), `--mic-deny` (the permission prompt says no), `--mic-prompt-secs N` (how long the
prompt takes), `--no-mic`, `--transcriber down`, `--tts down`.
On stdin: `push` (Fogar writes on its own), `dia` (a new day), `mic <state>`, `transcriber <state>`,
`tts <state>`, `silencio` / `seguridad` (cut the open take), `frase_perdida`. Launch arguments `-seccion emparejar` and `-apariencia clara|oscura|sistema` pick the
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

Golden JSON from the Rust tests, framing, the pairing code check, the app's links to the agent
(status and conversation) against a scripted agent on a real Unix socket, the conversation model
(take gestures, visible-turn rule, microphone states), and the stand-in's conversation rules.
