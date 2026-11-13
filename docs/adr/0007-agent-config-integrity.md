# ADR-0007: Agent-team configuration integrity

Status: **ACCEPTED** (slice 0 review fix sec#6)

## Context
No human reviews code: the agent definitions (`agent-team/agents/*.md`) and vendored skills
(`agent-team/skills/`) decide what the implementers and reviewers are allowed to do. A PR that
widens an agent's tools, adds a hook / MCP server / slash command, or edits a skill changes the
quality system itself.

## Decision
`scripts/check_vendored.sh` (gate "vendored skills hygiene", self-tested by
`scripts/test_check_vendored.sh`) enforces:
1. No executable agent configuration anywhere in the repo, case-insensitive: `hooks/`,
   `hooks.json`, `mcp.json`, `.mcp.json`, `settings*.json`, anything under `.claude/commands/`.
2. Hash lists: `agent-team/VENDORED.sha256` (skills) and `agent-team/AGENTS.sha256` (agent
   definitions); no file added, removed or changed. The installed copies under `.claude/` are
   checked the same way.
3. Frontmatter allowlists. Skills: `name`, `description` only. Agents: `name`, `description`,
   `model`, `tools`, `disallowedTools` (must be `Skill`), `skills` (max 2), `maxTurns`,
   `isolation`, `permissionMode` (`default` or `plan` only). Keys are matched case-insensitively
   with quotes stripped; any other top-level line fails.
4. Tools. Allowed: Read, Grep, Glob, Edit, Write, scoped `Bash(<prefix>:*)`. Unscoped `Bash` only
   with a `# unrestricted-bash: <reason>` line (today: orchestrator, perf-energy). Reviewers
   (`*-reviewer`): Read, Grep, Glob, `Bash(cargo test:*)`, `Bash(cargo clippy:*)`, and the
   read-only `Bash(git diff|log|show:*)` needed to see the diff under review.
5. The root hash `sha256(VENDORED.sha256 || AGENTS.sha256)` must equal the value recorded here:

ROOT_HASH: c80bc643f67c600e6358ae85ab245cb63a30258b646a5103db84ea66c704aa88

Changing a skill or an agent = reviewed edit, `scripts/rehash_agent_team.sh`, and update of the
line above in the same PR.

## Limits (honest)
- The hash lists and this ADR live in the same repository: a PR can regenerate all three, so
  the hashes only make changes *visible*; they do not prevent them. **CI must compare
  `ROOT_HASH` against the value on the main branch** and require an explicit human/orchestrator
  acknowledgement when it changes (not implemented in slice 0: there is no remote/CI run yet).
- `Bash(cargo test:*)` still executes the code under review (tests, build scripts). Reviewers
  must run it in a sandbox without secrets or network (the security-reviewer's `security-audit`
  helper scripts likewise).
- `Bash(cargo:*)` for implementers includes `cargo install`; the workspace is `--locked` and
  deps go through `[workspace.dependencies]` + deny.toml, but an agent could still run a tool it
  installed. Accepted for slice 0.
- The checks are text-level: they cannot see agent configuration outside this repository
  (user-level `~/.claude`, managed settings).
