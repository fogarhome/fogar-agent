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
1. No executable agent configuration anywhere in the repo (hidden directories included),
   case-insensitive: `hooks/`, `hooks.json`, `mcp.json`, `.mcp.json`, `settings*.json`.
   No `.claude*` entry (`.claude-plugin/`, `.claude.json`, `sub/.claude/`, ...) anywhere but
   the root `.claude/`, and the root `.claude/` holds only `agents/` and `skills/` (no
   `commands/`, `rules/`, `CLAUDE.md`, settings).
2. No symlinks, FIFOs, sockets or devices anywhere in the repo (working tree and tracked
   mode-120000 entries); none are expected. (Round-2 F1: a symlink escaped the hash lists.)
3. Hash lists: `agent-team/VENDORED.sha256` (skills) and `agent-team/AGENTS.sha256` (agent
   definitions); the listed path set must equal the file set exactly, and every hash must match.
   The installed copies `.claude/agents` / `.claude/skills` must be byte-identical to
   `agent-team/agents` / `agent-team/skills` (or empty); they carry no hash list of their own.
4. Frontmatter: `scripts/check_frontmatter.py` (Python 3 stdlib; the gate FAILS if `python3` is
   missing) parses a deliberately tiny grammar, not YAML: `---` exactly, then column-0
   `key: value` lines (or `# comment`), then `---` exactly. Rejected: BOM, CR, TAB, indentation,
   continuation lines, blank lines, duplicate keys, case variants, YAML-special leading
   characters (flow collections, anchors, tags, block scalars), escapes in quoted values. Only a
   skill's `description` may be a block scalar (`>-`, `>`, `|`, `|-`) with uniformly indented
   text lines. If PyYAML is importable, its parse must agree on the key set.
   Skills: exactly `name` (= directory name) and `description`. Agents: `name` (= file stem),
   `description`, `tools`, `disallowedTools` (exactly `Skill`) required; optional `model`
   (opus/sonnet/haiku/inherit), `skills` (max 2, each a vendored skill), `maxTurns`,
   `isolation` (worktree), `permissionMode` (`default` or `plan` only).
5. Tools. Allowed: Read, Grep, Glob, Edit, Write, and `Bash(<scope>:*)` for an explicitly
   allowlisted scope: cargo, cargo fmt/test/clippy/generate-lockfile/update, git
   status/diff/log/show/add/commit, scripts/gates.sh (never bash/sh/env/curl/...). Unscoped
   `Bash` only with a `# unrestricted-bash: <reason>` line (today: orchestrator, perf-energy).
   Reviewers are identified by FILE NAME (`*-reviewer.md`): Read, Grep, Glob,
   `Bash(cargo test:*)`, `Bash(cargo clippy:*)`, and the read-only `Bash(git diff|log|show:*)`.
6. The root hash `sha256(VENDORED.sha256 || AGENTS.sha256)` must equal the value recorded here:

ROOT_HASH: c80bc643f67c600e6358ae85ab245cb63a30258b646a5103db84ea66c704aa88

Changing a skill or an agent = reviewed edit, `scripts/rehash_agent_team.sh`, and update of the
line above in the same PR.

## Limits (honest)
- The hash lists and this ADR live in the same repository: a PR can regenerate all three, so
  the hashes only make changes *visible*; they do not prevent them. **CI must compare
  `ROOT_HASH` against the value on the main branch** and require an explicit human/orchestrator
  acknowledgement when it changes. Done in S1: the CI step "Agent-team ROOT_HASH unchanged vs
  base" fails a PR whose value differs from its base branch unless it carries the
  `agent-config-change` label.
- `Bash(cargo test:*)` still executes the code under review (tests, build scripts). Reviewers
  must run it in a sandbox without secrets or network (the security-reviewer's `security-audit`
  helper scripts likewise).
- `Bash(cargo:*)` for implementers includes `cargo install`; the workspace is `--locked` and
  deps go through `[workspace.dependencies]` + deny.toml, but an agent could still run a tool it
  installed. Accepted for slice 0.
- The checks are text-level: they cannot see agent configuration outside this repository
  (user-level `~/.claude`, managed settings).
- `CLAUDE.md` files (root or nested) are instruction text, not configuration, and are not
  checked; they cannot grant tools or permissions, but they do steer agents. Review them as code.
- The checker runs on the working tree (minus `target/` and `.git/`) plus `git ls-files`; it
  cannot see what a later checkout/merge brings in before it runs.
