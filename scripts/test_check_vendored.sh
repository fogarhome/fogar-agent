#!/usr/bin/env bash
# Self-test for scripts/check_vendored.sh (review sec#6): plants each forbidden configuration in a
# temporary copy of the repo's agent config and asserts the checker rejects it; the pristine copy
# must pass. Run by scripts/gates.sh.
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/fogar-vendored-test.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
fail=0

fresh() { # -> path of a pristine copy
  local d="$WORK/case"
  rm -rf "$d"; mkdir -p "$d/docs"
  cp -R "$ROOT/scripts" "$ROOT/agent-team" "$d/"
  cp -R "$ROOT/docs/adr" "$d/docs/"
  [ -d "$ROOT/.claude" ] && cp -R "$ROOT/.claude" "$d/"
  echo "$d"
}

expect() { # expect pass|fail "description" (runs the checker in $D)
  local want="$1" what="$2" out rc
  out="$("$D/scripts/check_vendored.sh" 2>&1)"; rc=$?
  if { [ "$want" = pass ] && [ $rc -ne 0 ]; } || { [ "$want" = fail ] && [ $rc -eq 0 ]; }; then
    echo "SELFTEST FAIL: expected $want: $what"; echo "$out" | sed 's/^/    /'; fail=1
  else
    echo "selftest ok ($want): $what${out:+ -> $(echo "$out" | head -1)}"
  fi
}

# Re-hash the copy and record the new root hash in its ADR, i.e. what a PR that also regenerates
# the hash lists would do: the frontmatter rules must still fire on their own.
rehash() {
  local r; r="$("$D/scripts/rehash_agent_team.sh")"
  sed -i "s/^ROOT_HASH: .*/ROOT_HASH: $r/" "$D/docs/adr/0007-agent-config-integrity.md"
}
agent_fm() { # agent_fm file 'sed expression' (edits an agent definition, then re-hashes)
  sed -i "$2" "$D/agent-team/agents/$1"; rehash
}

D="$(fresh)"; expect pass "pristine copy"

D="$(fresh)"; agent_fm audio.md 's/^tools: .*/tools: "*"/'; expect fail 'tools: "*"'
D="$(fresh)"; agent_fm audio.md 's/^tools: .*/tools: Read, WebFetch, Bash(cargo:*)/'; expect fail "tool outside the allowlist (WebFetch)"
D="$(fresh)"; agent_fm audio.md 's/^maxTurns:/permissionMode: bypassPermissions\nmaxTurns:/'; expect fail "permissionMode bypassPermissions"
D="$(fresh)"; agent_fm audio.md 's/^maxTurns:/permissionMode: acceptEdits\nmaxTurns:/'; expect fail "permissionMode acceptEdits"
D="$(fresh)"; agent_fm audio.md 's/^maxTurns:/"hooks": {}\nmaxTurns:/'; expect fail 'quoted "hooks" key'
D="$(fresh)"; agent_fm audio.md "s/^maxTurns:/'mcpServers': [x]\nmaxTurns:/"; expect fail "quoted mcpServers key"
D="$(fresh)"; agent_fm audio.md 's/^maxTurns:/MCPSERVERS: [x]\nmaxTurns:/'; expect fail "upper-case mcpServers key"
D="$(fresh)"; agent_fm audio.md 's/^maxTurns:/color: red\nmaxTurns:/'; expect fail "unknown frontmatter key"
D="$(fresh)"; agent_fm code-reviewer.md 's/^tools: .*/tools: Read, Grep, Glob, Bash/'; expect fail "reviewer with unrestricted Bash"
D="$(fresh)"; agent_fm security-reviewer.md 's/^tools: .*/tools: Read, Grep, Glob, Edit/'; expect fail "reviewer with Edit"
D="$(fresh)"; agent_fm tester.md 's/^tools: .*/tools: Read, Grep, Glob, Edit, Write, Bash/'; expect fail "unjustified unrestricted Bash"
D="$(fresh)"; echo "extra line" >> "$D/agent-team/agents/tester.md"; expect fail "agent body changed (hash)"
D="$(fresh)"; agent_fm audio.md 's/^description:/# just a comment\ndescription:/'; expect pass "re-hashed harmless edit passes (rehash helper works)"
D="$(fresh)"; cp "$D/agent-team/agents/tester.md" "$D/agent-team/agents/rogue.md"; expect fail "agent added (hash list)"
D="$(fresh)"; sed -i 's/^name: fogar-secure-rust/name: fogar-secure-rust\n"allowed-tools": Bash/' "$D/agent-team/skills/fogar-secure-rust/SKILL.md"; rehash; expect fail "quoted allowed-tools in a skill (re-hashed)"
D="$(fresh)"; sed -i 's/^name: fogar-secure-rust/name: fogar-secure-rust\nHooks: {}/' "$D/agent-team/skills/fogar-secure-rust/SKILL.md"; rehash; expect fail "hooks key in a skill (re-hashed)"
D="$(fresh)"; echo "x" >> "$D/agent-team/skills/fogar-secure-rust/SKILL.md"; expect fail "skill body changed (hash)"
D="$(fresh)"; echo "x" >> "$D/agent-team/skills/fogar-secure-rust/SKILL.md"; "$D/scripts/rehash_agent_team.sh" >/dev/null; expect fail "hash lists regenerated but ROOT_HASH not updated"
D="$(fresh)"; mkdir -p "$D/.claude/commands"; echo x > "$D/.claude/commands/deploy.md"; expect fail ".claude/commands/*"
D="$(fresh)"; mkdir -p "$D/docs/x"; echo '{}' > "$D/docs/x/hooks.json"; expect fail "hooks.json anywhere"
D="$(fresh)"; echo '{}' > "$D/docs/MCP.json"; expect fail "MCP.json (case-insensitive)"
D="$(fresh)"; echo '{}' > "$D/docs/.mcp.json"; expect fail ".mcp.json"
D="$(fresh)"; mkdir -p "$D/.claude"; echo '{}' > "$D/.claude/Settings.Local.JSON"; expect fail "settings*.json (case-insensitive)"
D="$(fresh)"; sed -i 's/^\(ROOT_HASH: \).*/\1deadbeef/' "$D/docs/adr/0007-agent-config-integrity.md"; expect fail "root hash not the recorded one"

[ "$fail" -eq 0 ] && echo "check_vendored self-test: ok"
exit "$fail"
