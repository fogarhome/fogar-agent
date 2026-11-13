# Threat model: always-on mic + screen agent (fogar-agent)

Use STRIDE-per-boundary. Update this file when a boundary or capability is added.

## Assets (ranked)

1. Live microphone audio and screen contents (and anything derived: transcripts, OCR, thumbnails).
2. Gateway credentials / device identity; SSH keys and passphrases; other stored secrets.
3. Ability to execute actions on the machine (SSH, file access, any remote command handler).
4. Integrity of the agent binary, updates, and configuration (policy: when capture is allowed).
5. Availability of the agent (always-on; crash loops are user-visible and a DoS vector).

## Actors

| Actor | Starting capability | Primary worry |
|---|---|---|
| Remote network attacker | Can reach network path to gateway | TLS downgrade/MITM, token theft |
| Malicious/compromised gateway or account | Sends any WebSocket message | Turning the agent into a spy/RCE tool beyond local policy |
| Other local user | Separate uid on same Mac | Read socket/files/logs/Keychain items |
| Malicious same-user app | Runs as the user, no TCC grants | Use our socket to get capture/SSH/secrets (confused deputy); inject into our process |
| Malicious dependency / build tool | Code at build or run time | Exfiltration, backdoor in release |
| AI agent developer (us) | Writes code without human review | Introduces insecure defaults, debug bypasses, logs secrets |
| Physical/forensic | Disk, backups, swap | Recover captured data or secrets |

## Boundaries and required controls

| Boundary | Threats (STRIDE) | Controls (checklist IDs) |
|---|---|---|
| Gateway ⇄ agent (WSS) | Spoofing gateway, tampering, info disclosure, elevation via commands | NET-1..4, allowlisted commands, local policy beats remote |
| UI ⇄ agent (Unix socket) | Spoofed UI, elevation by other apps | IPC-1..5 (uid + code identity) |
| Agent ⇄ OS capture APIs | Capture without consent, capture continues after revoke | MAC-2, CAP rules below |
| Agent ⇄ Keychain/disk | Secret disclosure, tampering with config | SEC-1..4, LOG-3, 0700 dirs |
| Agent ⇄ SSH hosts | MITM, command injection, malicious output | SSH-1..3, INPUT fuzzing |
| Rust ⇄ C/ObjC (FFI) | Memory corruption from bad assumptions | UNSAFE-1..6 |
| Source ⇄ build ⇄ release | Supply-chain injection, unsigned/altered binary | SUPPLY gates, MAC-1, MAC-3 |

## CAP rules (capture-specific)

- CAP-1 Capture is off by default; enabling requires local, user-visible consent stored locally;
  the gateway can request, never force.
- CAP-2 Single choke point module decides "may capture now?" (policy + TCC status + user state);
  every capture path calls it; reviewers reject capture code that bypasses it.
- CAP-3 Captured buffers: minimal retention, never logged, never written to disk unencrypted,
  zeroized/dropped promptly; upload only over the authenticated channel with size caps.
- CAP-4 Visible indicator while capturing; audit log of start/stop events (names and times only).
- CAP-5 Kill switch: local user can stop capture instantly even if the gateway is unreachable.

## Abuse cases to test (each should have a test or a documented manual check)

1. Unsigned process running as the same user connects to the socket and sends "start capture".
2. Gateway sends a 2 GiB frame / deeply nested JSON / unknown command / replayed command.
3. Gateway (compromised) sends SSH command with shell metacharacters.
4. TLS interception proxy with a user-installed root (decide: platform trust or pin — document).
5. Agent started when socket path is a symlink to another file.
6. `RUST_LOG=trace` in release: does any secret or capture data appear?
7. Permission revoked mid-capture; screen locked; user switched.
8. New dependency with `build.rs` doing network access.
9. Panic inside an FFI callback.
10. Downgrade: older signed build offered by the updater.
