# Gateway WebSocket, TLS, tokens, SSH

Labels: [primary] checked 2026-11-13 in crate source; [secondary]; [unverified].

## TLS

- `rustls` 0.23.x (0.23.46 current) [primary: crates.io]. Root store: `rustls-platform-verifier`
  0.7.1 uses the OS trust store; `ClientConfig::with_platform_verifier()` via the
  `ConfigVerifierExt` trait, or `BuilderVerifierExt` on the builder [primary].
- Optional SPKI pin for the gateway as defence in depth; ship a backup pin and a rotation plan,
  otherwise a pin becomes an outage.
- Red flags in review: a custom `ServerCertVerifier` returning success, `.dangerous()` config,
  `danger_accept_invalid_certs` (reqwest/native-tls), feature flags enabling insecure modes in
  release, TLS disabled for "localhost testing" but reachable by config.

## WebSocket (`tokio-tungstenite` 0.30.0 / `tungstenite` 0.30.0)

- Connect with `connect_async_tls_with_config(request, Some(config), disable_nagle,
  Some(Connector::Rustls(Arc<ClientConfig>)))` so the TLS config is ours, not a default [primary:
  fn and `Connector::Rustls` exist].
- `WebSocketConfig` is `#[non_exhaustive]`; set limits with builder methods
  `.max_message_size(Some(n))` and `.max_frame_size(Some(n))` [primary]. Choose limits per message
  type; large uploads (audio/screen) go over a separate, authenticated, size-capped channel.
- Auth: send the token in a header (`Authorization`) on the upgrade request, not the query string.
  Tokens short-lived and scoped; refresh over the authenticated channel; on 401/close codes, do not
  loop instantly (backoff + jitter, max rate).
- Server → agent commands are untrusted: closed enum, schema-validated, allowlisted actions; the
  gateway must not be able to enable capture without local policy permitting it (local policy beats
  remote command). Log action *names*, never payloads.
- Replay: commands that change state carry a nonce/sequence; agent rejects duplicates and stale ones.

## SSH

- Prefer `russh` 0.65.1 (pure Rust) or `ssh2` 0.9.7 (libssh2 FFI → falls under UNSAFE review)
  [primary: versions].
- `russh::client::Handler::check_server_key(&mut self, &PublicKeyOrCertificate)` — the default
  implementation **rejects all keys** ("you must override it") [primary]. Implement against a
  pinned key or known_hosts store (`russh` has `keys::known_hosts` module [primary: file exists]).
  `Ok(true)` unconditionally is a Critical finding (MITM).
- Keys: from Keychain or ssh-agent; passphrase-protected private keys decrypted into `Zeroizing`
  buffers; no agent forwarding unless a ticket explicitly needs it.
- Command construction: fixed program + argument vector from an allowlist. Remember the remote side
  runs a shell: arguments that come from the gateway must be validated against a strict pattern, not
  escaped ad hoc.
- Timeouts on connect/auth/exec; cap output size read back; treat remote output as untrusted input
  (parsers fuzzed, never interpreted as commands or log format strings).

## Outbound HTTP (if any)

Same TLS rules; no redirects to non-HTTPS; no proxy auto-config surprises; bounded response size.
