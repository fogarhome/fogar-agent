# Secrets, Keychain, zeroization, log redaction

Labels: [primary] checked 2026-11-13 in crate source; [secondary]; [unverified].

## Storage

- macOS Keychain via `security-framework` 3.7.0: `passwords::{set_generic_password,
  get_generic_password, delete_generic_password}` and the `*_options` variants taking
  `PasswordOptions` [primary]. Use the options variant when you need access-control flags.
- `keyring` 4.2.0 is now a thin wrapper/sample over `keyring-core` plus per-store crates; the
  README tells applications to depend on `keyring-core` and the specific store crates [primary:
  README]. Prefer `security-framework` directly on macOS unless cross-platform parity is needed.
- Keychain items are bound to the code signature that created them; re-signing with another
  identity or ad-hoc builds changes access behaviour [secondary]. Test with the release signature.
- Never: secrets in env vars passed to children, CLI args (visible in `ps`), plist/config files,
  crash reports, or the Unix-socket protocol.

## In-memory handling

- `secrecy` 0.10.3: `SecretBox<S: Zeroize>`, `SecretString = SecretBox<str>`,
  `SecretSlice<S>`; access only via `ExposeSecret::expose_secret()`; its `Debug` prints
  `[REDACTED]` [primary].
- `zeroize` 1.9.1: `Zeroize`, `ZeroizeOnDrop`, `Zeroizing<T>`; derive macros behind the `derive`
  feature [primary].
- Limits (state them in reviews, do not over-claim): zeroize clears the current buffer only; moves,
  `Vec`/`String` growth reallocations, `clone()`, serialization buffers, TLS library internals, and
  swap can leave copies [secondary]. Mitigate: pre-size buffers, avoid `Clone`, keep secrets in
  `Box`ed fixed-size storage, minimize lifetime. For high assurance, run Trail of Bits
  `zeroize-audit` (checks IR/asm for optimized-away wipes) [primary: skill read].
- Comparison: `subtle` 2.6.1 `ConstantTimeEq` for tokens/MACs [secondary: API name].

## Logging / redaction (LOG)

- Types holding secrets: no `Display`; `Debug` redacted (secrecy does this). For other structs with
  secret fields, hand-write `Debug` or use a newtype — never `#[derive(Debug)]` on a struct with a
  raw `String` token.
- `tracing` 0.1.44: do not record secrets as span fields; `#[instrument(skip(token, ...))]` or
  `skip_all` on functions that take secrets/capture buffers [secondary: attribute options].
- Error messages from HTTP/TLS/WebSocket/SSH crates can include URLs, headers, or server banners;
  map to our own error enum at the boundary and log the enum.
- Never log audio/screen bytes, OCR/transcripts, clipboard, or file contents captured for the user.
- Redaction test (required): construct each config/request/error type with a sentinel secret
  (e.g. `"SENTINEL-7f3a"`), format with `{:?}` and `{}` where implemented, and through the log
  subscriber into a buffer; assert the sentinel never appears.
- Log files: private dir, `0600`, size-bounded rotation; release builds default to `info` with no
  request bodies.

## Panics and crash output

Panic messages may include values (`unwrap()` on a `Result` prints the `Err` via `Debug`). Ensure
error types carrying secrets redact in `Debug`, and set a panic hook that logs location only.
