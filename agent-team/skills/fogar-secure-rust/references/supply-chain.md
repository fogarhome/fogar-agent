# Supply-chain gates (SUPPLY)

Labels: [primary] checked 2026-11-13; [secondary]; [unverified].

Current tool versions (crates.io, 2026-11-13) [primary]: cargo-deny 0.20.2, cargo-audit 0.23.0,
cargo-vet 0.10.3, cargo-geiger 0.14.0, cargo-auditable 0.7.7, cargo-fuzz 0.13.2.
Install CI tools with `cargo install --locked <tool> --version <exact>` (or a pinned prebuilt with
checksum); never unpinned `curl | sh` or `npx -y`.

## cargo-deny (0.20.x config format)

Deprecated keys that silently stop meaning anything: `[advisories] vulnerability`, `notice`;
`[licenses] unlicensed`, `copyleft`, `allow-osi-fsf-free`, `default`, `deny` [primary:
`src/advisories/cfg.rs`, `src/licenses/cfg.rs`]. Vulnerability advisories are always errors.
`unmaintained`/`unsound` take a scope: `"all" | "workspace" | "transitive" | "none"` [primary].

```toml
[graph]
targets = ["aarch64-apple-darwin", "x86_64-apple-darwin"]   # add Windows/Linux when ported

[advisories]
unmaintained = "all"
unsound = "all"
yanked = "deny"
ignore = [
  # { id = "RUSTSEC-YYYY-NNNN", reason = "why, link to issue, expiry date" },
]

[licenses]
allow = ["MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause", "BSD-3-Clause", "ISC", "Unicode-3.0"]
confidence-threshold = 0.9

[bans]
multiple-versions = "warn"
wildcards = "deny"
deny = [
  { crate = "openssl", reason = "use rustls" },
]

[sources]
unknown-registry = "deny"
unknown-git = "deny"
allow-registry = ["https://github.com/rust-lang/crates.io-index"]
allow-git = []
```
Adjust the license allowlist to what `cargo deny check licenses` actually reports; every addition
needs a reason in the PR.

## cargo-audit / cargo-auditable

- `cargo audit` in CI against `Cargo.lock` (RustSec DB) [secondary].
- Build release binaries with `cargo auditable build --release --locked` so shipped binaries embed
  the dependency list and can be audited later (`cargo audit bin <binary>`) [secondary].

## cargo-vet

- `cargo vet init` once; commit `supply-chain/{config.toml,audits.toml,imports.lock}` [secondary].
- Import audits from trusted orgs (e.g. Mozilla, Google, Bytecode Alliance) in `config.toml` [secondary].
- New/updated dep → `cargo vet` fails → a reviewer agent must either `cargo vet certify` after an
  actual diff review (`cargo vet diff`/`inspect`) or record an exemption with a reason [secondary].
- Policy: agents may **not** add exemptions for crates that contain `unsafe`, `build.rs`, or
  proc-macros without a security-reviewer sign-off in the PR.

## What a reviewer checks on any `Cargo.toml`/`Cargo.lock` change

1. Why is the dependency needed? Could std or an existing dep do it?
2. Owner/maintenance (last release, RustSec status), download count sanity, typosquat names.
3. `build.rs` and proc-macros run at build time with developer/CI privileges — read them.
4. Features: disable default features; no `native-tls`/`openssl` sneaking in; no insecure-mode features.
5. `cargo geiger` delta for new deps with `unsafe`.
6. Git or path dependencies: forbidden in release unless allowlisted with a pinned rev.
7. Lockfile churn: unexpected transitive bumps in an unrelated PR are a red flag.

## CI / agent-environment hygiene

- `Cargo.lock` committed; all builds `--locked`; `rust-toolchain.toml` pins the toolchain
  (nightly jobs pin a date).
- GitHub Actions: pin actions by full commit SHA; least-privilege `permissions:`; no
  `pull_request_target` with checkout of PR code.
- Agent tooling: no MCP servers or hooks installed from unpinned sources; skills copied at pinned
  commits and reviewed (see REPORT.md).
