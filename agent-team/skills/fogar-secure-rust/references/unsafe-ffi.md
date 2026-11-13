# Unsafe, FFI, dynamic analysis, fuzzing

Labels: [primary] checked 2026-11-13 against source/docs; [secondary] widely documented, not re-checked; [unverified] confirm first.

## Review procedure for an `unsafe` block

1. Find every safe entry point that transitively reaches the block (public fns, trait impls,
   callbacks registered with C/ObjC). The block is only as sound as the weakest caller path.
2. For each raw pointer: who allocated it, which allocator frees it, is it non-null, aligned,
   initialized, and valid for the whole use (no use after the owning Rust value drops)?
3. Slices from raw parts: `len` is in elements not bytes; `len * size_of::<T>() <= isize::MAX`;
   memory initialized; no concurrent mutation by the C side.
4. Strings: `CStr::from_ptr` needs a NUL inside the allocation; `CString::as_ptr()` on a temporary
   dangles (`CString::new(x).unwrap().as_ptr()` is a classic UAF).
5. `transmute`/pointer casts: layout must be `#[repr(C)]`/`#[repr(transparent)]`; enums from C
   integers must be range-checked before conversion (invalid discriminant = UB).
6. Panics across FFI: an `extern "C"` Rust fn that panics aborts the process on current Rust
   (unwinding out of a non-`-unwind` ABI aborts) [secondary]. Wrap bodies with
   `std::panic::catch_unwind` where an abort is unacceptable and return an error code.
7. Threads: AppKit/UI objects on the main thread only; CoreFoundation objects shared across threads
   only when documented thread-safe; `unsafe impl Send/Sync` needs a written argument.
8. CoreFoundation ownership: "Create/Copy" functions return +1 references the caller must release;
   "Get" functions do not [secondary]. Prefer wrapper types (`core-foundation` `TCFType` wrappers,
   `objc2` `Retained<T>`) that encode ownership instead of manual retain/release.
9. Drop order and `mem::forget`: forgetting a guard can skip unlock/zeroize; check every early return.

## Crate hygiene (MUST in adapter crates)

```rust
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]
```
Non-adapter crates: `#![forbid(unsafe_code)]`. These lints exist in rustc/clippy [secondary].

## Dynamic analysis

| Tool | Use | Command (pin toolchain in `rust-toolchain.toml` for nightly jobs) |
|---|---|---|
| Miri | UB in pure-Rust unsafe (aliasing, uninit, OOB, leaks). Cannot execute real FFI calls; isolate FFI behind traits and test the Rust side | `cargo +nightly miri test -p <adapter-crate>` [secondary] |
| AddressSanitizer | heap/stack OOB, UAF in tests that call real C/ObjC | `RUSTFLAGS="-Zsanitizer=address" cargo +nightly test --target aarch64-apple-darwin` [secondary] |
| ThreadSanitizer | data races across FFI/threads | `-Zsanitizer=thread` plus `-Zbuild-std` usually required [unverified on current nightly] |
| cargo-geiger 0.14.0 | count `unsafe` in deps; track per release | `cargo geiger` [secondary] |

Sanitizers are nightly-only flags; record the exact nightly date used in CI.

## Fuzzing (INPUT checklist)

Fuzz every parser that sees bytes from: the gateway WebSocket, the Unix socket, SSH output we parse,
config files, and any FFI return buffers we decode.

- cargo-fuzz 0.13.2 + libfuzzer-sys 0.4.13; structured inputs with `arbitrary` 1.5.0 [primary: versions].
- `cargo +nightly fuzz init`, `cargo +nightly fuzz add <name>`, `cargo +nightly fuzz run <name>` [secondary].
- Harness rule: call the same function production calls; no `unwrap()` in the harness that hides
  a real panic; assert round-trip properties where a serializer exists.
- Treat any panic reachable from remote input as at least Medium (remote DoS of an always-on agent).
- Keep corpora in-repo (small) and minimized crash inputs as regression tests.

## Untrusted-input rules for safe Rust

- Every length/count field from the wire is bounded before allocation (`Vec::with_capacity(n)` with
  attacker `n` is a memory DoS).
- Recursion depth bounded for nested formats (serde_json has a default recursion limit
  [secondary]; custom recursive parsers need their own).
- Arithmetic on wire values uses `checked_*`; release builds wrap silently by default.
- Paths: reject absolute paths and `..`; open relative to a directory handle; do not check-then-open.
- Avoid `unwrap()`/`expect()`/indexing on data from outside; return errors.

## Deep audit option

Trail of Bits `rust-review` (see REPORT.md) covers 69 Rust bug classes incl. FFI, unsound safe
APIs, Send/Sync, panic-DoS, TOCTOU [primary: manifest.json read]. Use for milestone audits, not every PR.
