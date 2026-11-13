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
