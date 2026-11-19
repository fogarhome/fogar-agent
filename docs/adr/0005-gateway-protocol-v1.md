# ADR-0005: Implement gateway client protocol v1 ("v1 with fixes"); v2 does not exist

Status: **PROVISIONAL**

## Decision
`fogar-proto` models exactly what runs today (PROTOCOL-SPEC.md / RUST-MODEL.md, vault snapshot
2026-11-13): handshake JSON without `type`, `ready`, `send`/`end`/`cancel`/`keepalive`,
`turn_start`/`token`/`turn_end`/`interrupted`/`status`/`error`/`transcription*`, binary upload
f32le 16 kHz mono, TTS frames `0xA1 + turn_seq(u16 BE) + PCM16 LE 24 kHz`, close classification
by code + reason string. Tolerant reader (`Unknown` variants, unknown fields ignored), strict
writer. Nothing from the v2 proposal (`hello`, `0xA2`, 4xxx closes, `bye`, `ping`) is modelled.

GUESSED / UNKNOWN items kept loose, never invented: `pipeline_event` beyond `stage`/`turn`;
`tool_call.args` (raw JSON); `INVALID_MESSAGE` text; silence-watchdog close code (assumed 1000);
order of `error` vs `turn_end`; first `turn_seq` value.

Our policies (not from the gateway): key-invalid/revoked => park (`Repair`, key NOT deleted in
S0); agent refused / client bug => park; 1000 idle => prompt jittered reconnect; 1011/1013/1006/
unknown => capped exponential backoff with full jitter; keepalive after 120 s client silence;
TTS frames gated by `turn_seq` equality; local barge-in sends `cancel` and drops the cancelled
seq. A `turn_start` we did not ask for is treated as a push (heuristic: v1 has no `origin`).

## Known gaps (protocol, not ours to invent)
No application ping => half-open detection needs WS pings in S1; no message ids => a `send`
retried after reconnect may duplicate; no desktop `client_type`.

## To verify (against the real gateway, from any machine incl. the M2)
- Golden JSON fixtures from the gateway repo's `docs/client-protocol.md` / e2e harness replace the
  vault-derived vectors (turns every LIKELY/GUESSED mark into CERTAIN or fixes it).
- Real close reasons byte-for-byte (Spanish strings, accents) and the silence-watchdog code.

## Amendment (2026-11-15): gateway M19 ("v3") compatibility
The gateway's M19 (conversations, OpenClaw routing; branch `claude/hopeful-pasteur-phx8ad`, not yet
released at the time; released as 3.0.0 on gateway main, E2E pinned to 3.0.1 since 2026-11-19) changes the client protocol. The agent speaks BOTH generations until every gateway is on
M19:
- Closes: M19 gives each application reason its own 4xxx code and the code is the contract
  (`fogar_proto::close::classify_close`): 4010/4200 re-pair, 4020/4021 agent refused, 4102
  conversation taken (park, never auto-reconnect), other 4000–4899 park, 4900–4999 backoff. The
  old 1008 + text mapping stays for gateways <= 2.x.
- `turn_end.reason` (done/error/cancelled/interrupted/replaced/timeout/closed; unknown = error)
  replaces the `interrupted` message; `interrupted`/`replaced` stop playback. The old message is
  still handled.
- `turn_start.origin` (user/push) is trusted when present; otherwise inferred as before.
- Not adopted yet (conversations slice): the handshake `conversation` field (we omit it, so the
  gateway uses the "conversation of the day"), `ready.conversation`, `turn_start.conversation`
  (decoded, unused), `turn_progress` (we never request `status`).
The mock gateway speaks M19 by default (`MockConfig::m19 = false` plays a 2.x gateway).
