# Vendored skills (inert text; integrity-checked by `scripts/check_vendored.sh`)

Staged here because writing `.claude/` needs explicit user approval in the build session. Review,
then install with `scripts/install_agent_team.sh` (copies `skills/` and `agents/` into `.claude/`).

| Skill dir | Source | Version / commit | Licence |
|---|---|---|---|
| `fogar-hexagonal-slices` | project research bundle (`scratchpad/skills/`), 2026-11-12/13 | unversioned internal draft; content pinned by `VENDORED.sha256` | project-internal (no external licence) |
| `fogar-longrun-reliability` | same | same | project-internal |
| `fogar-energy-budget` | same (`dl-energy/` third-party sources NOT copied) | same | project-internal |
| `fogar-realtime-audio` | same | same | project-internal |
| `fogar-system-probe` | same | same | project-internal |
| `fogar-macos-daemon` | same | same | project-internal |
| `fogar-secure-rust` | `scratchpad/secskills/fogar-secure-rust` (draft, see secskills/REPORT.md) | same | project-internal |
| `security-audit` | https://github.com/cloudflare/security-audit-skill, path `skills/security-audit` only (`git archive`) | `c1c8a8c1471069fb0e188eeaff69b8e8db6564a8` (2026-09-14) | MIT (`security-audit/LICENSE`, Copyright 2025-2026 Cloudflare, Inc.) |

Notes:
- `security-audit` ships `validate-*.cjs` helper scripts. They are inert unless an agent runs them;
  the security-reviewer may run them only inside a sandboxed audit job (secskills/REPORT.md).
  The upstream README's `npx skills add` installer is NOT used.
- The fogar skills contain reference Rust projects (`references/example-workspace`,
  `references/examples`). They are not workspace members and are never built by the gates.
- See ADR-0007 for the full integrity rules (agent hashes, frontmatter allowlists, root hash).
- No vendored directory may contain `hooks/`, `.mcp.json`, `settings*.json` or broad
  `allowed-tools` (enforced). Updating a skill = re-vendor + regenerate `VENDORED.sha256` in a
  reviewed PR.
- `rust-async-internals` (po4yka) was NOT vendored; core-async uses fogar-longrun-reliability +
  fogar-hexagonal-slices instead.
