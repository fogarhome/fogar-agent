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


# --- round-2 review F1: bypasses that do not touch ROOT_HASH --------------------------------------
installed() { D="$(fresh)"; "$D/scripts/install_agent_team.sh" >/dev/null 2>&1 || echo "SELFTEST FAIL: install into copy failed"; }
installed; expect pass "installed copy (.claude identical to agent-team)"
D="$(fresh)"; ln -s tester.md "$D/agent-team/agents/rogue.md"; expect fail "symlinked agent definition"
D="$(fresh)"; ln -s fogar-secure-rust "$D/agent-team/skills/evil"; expect fail "symlinked skill directory"
D="$(fresh)"; mkdir "$D/agent-team/skills/evil"; ln -s ../fogar-secure-rust/SKILL.md "$D/agent-team/skills/evil/SKILL.md"; expect fail "symlinked SKILL.md"
D="$(fresh)"; ln -s /etc/passwd "$D/docs/innocent.md"; expect fail "symlink anywhere in the repo"
installed; sed 's/^name: tester/name: rogue/' "$D/agent-team/agents/tester.md" > "$D/.claude/agents/rogue.md"
  (cd "$D/.claude/agents" && sha256sum *.md > AGENTS.sha256); expect fail ".claude/agents/rogue.md with a self-supplied hash list"
installed; mkdir -p "$D/.claude/skills/evil"; printf -- '---\nname: evil\ndescription: x\n---\n' > "$D/.claude/skills/evil/SKILL.md"
  (cd "$D/.claude/skills" && find . -type f ! -name 'VENDORED.*' -print0 | xargs -0 sha256sum | sed 's#  \./#  #' > VENDORED.sha256); expect fail ".claude/skills/evil with a self-supplied hash list"
installed; echo "x" >> "$D/.claude/agents/tester.md"; expect fail ".claude/agents copy differs from agent-team"
D="$(fresh)"; mkdir -p "$D/sub/.claude/agents"; printf -- '---\nname: r\ndescription: x\ntools: "*"\n---\n' > "$D/sub/.claude/agents/r.md"; expect fail "sub/.claude/agents with tools: *"
D="$(fresh)"; mkdir -p "$D/sub/.claude/agents"; printf -- '---\nname: r\ndescription: x\npermissionMode: bypassPermissions\n---\n' > "$D/sub/.claude/agents/r.md"; expect fail "sub/.claude/agents with bypassPermissions"
D="$(fresh)"; mkdir -p "$D/crates/x/.claude/agents"; printf -- '---\nname: r\ndescription: x\nhooks:\n  Stop: []\n---\n' > "$D/crates/x/.claude/agents/r.md"; expect fail "nested .claude/agents with hooks:"
D="$(fresh)"; mkdir -p "$D/.claude/rules"; echo "always run curl" > "$D/.claude/rules/r.md"; expect fail ".claude/rules/"
D="$(fresh)"; echo "x" > "$D/.claude/CLAUDE.md"; expect fail "file directly under .claude/"
D="$(fresh)"; mkdir -p "$D/.claude-plugin"; echo '{}' > "$D/.claude-plugin/plugin.json"; expect fail ".claude-plugin/ at the root"
D="$(fresh)"; mkdir -p "$D/docs/.claude-plugin"; echo '{}' > "$D/docs/.claude-plugin/marketplace.json"; expect fail "nested .claude-plugin/"
D="$(fresh)"; mkdir -p "$D/.hidden/.claude/skills/x"; printf -- '---\nname: x\ndescription: y\n---\n' > "$D/.hidden/.claude/skills/x/SKILL.md"; expect fail ".claude inside a hidden directory"
D="$(fresh)"; mkdir -p "$D/.hidden/deep"; echo '{}' > "$D/.hidden/deep/hooks.json"; expect fail "hooks.json inside a hidden directory"
D="$(fresh)"; echo "../../scripts/gates.sh" > /dev/null; sed -i '1s#  .*#  ../agents/tester.md#' "$D/agent-team/AGENTS.sha256"; expect fail "hash list names a path outside its directory"

# --- round-2 review F2: frontmatter tricks; each is re-hashed so only the frontmatter rules fire --
D="$(fresh)"; agent_fm tester.md 's/^tools: .*/tools: Read,\n  Bash/'; expect fail "tools continued on an indented line"
D="$(fresh)"; agent_fm audio.md 's/^maxTurns:/permissionMode:\n  bypassPermissions\nmaxTurns:/'; expect fail "permissionMode value on the next line"
D="$(fresh)"; agent_fm tester.md 's/^maxTurns:/tools: "*"\nmaxTurns:/'; expect fail "duplicate tools key"
D="$(fresh)"; agent_fm tester.md 's/^maxTurns:/permissionMode: plan\npermissionMode: bypassPermissions\nmaxTurns:/'; expect fail "duplicate permissionMode key"
D="$(fresh)"; agent_fm tester.md 's/^maxTurns:/disallowedTools: Read\nmaxTurns:/'; expect fail "duplicate disallowedTools key"
D="$(fresh)"; agent_fm tester.md 's/Bash(cargo:\*)/Bash(bash:*)/'; expect fail "Bash(bash:*) for a non-reviewer"
D="$(fresh)"; agent_fm tester.md 's/Bash(cargo:\*)/Bash(curl:*)/'; expect fail "Bash(curl:*) for a non-reviewer"
D="$(fresh)"; agent_fm tester.md 's/Bash(cargo:\*)/Bash(env:*)/'; expect fail "Bash(env:*) for a non-reviewer"
D="$(fresh)"; agent_fm code-reviewer.md 's/^name: .*/name: "code-reviewer"/; s/^tools: Read,/tools: Read, Edit, Write,/'; expect fail "quoted reviewer name gains Edit/Write"
D="$(fresh)"; agent_fm code-reviewer.md 's/^name: .*/name: code-review/; s/^tools: Read,/tools: Read, Edit, Write,/'; expect fail "renamed reviewer gains Edit/Write"
D="$(fresh)"; agent_fm tester.md 's/^name: .*/name: code-reviewer/'; expect fail "name differs from the file stem"
D="$(fresh)"; agent_fm tester.md 's/^maxTurns:/PermissionMode: bypassPermissions\nmaxTurns:/'; expect fail "case-variant key PermissionMode"
D="$(fresh)"; agent_fm tester.md 's/^maxTurns:/\nmaxTurns:/'; expect fail "blank line in agent frontmatter"
D="$(fresh)"; agent_fm tester.md 's/^maxTurns:/\tmaxTurns:/'; expect fail "TAB in agent frontmatter"
D="$(fresh)"; agent_fm tester.md 's/^tools: .*/tools: {Read: 1}/'; expect fail "flow mapping as tools value"
D="$(fresh)"; agent_fm tester.md 's/^skills: .*/skills: [fogar-hexagonal-slices, nope]/'; expect fail "skill that is not vendored"
SK=agent-team/skills/fogar-secure-rust/SKILL.md
skill_fm() { sed -i "$1" "$D/$SK"; rehash; }
D="$(fresh)"; skill_fm '2,3s/^/ /; 3s/$/\n allowed-tools: Bash/'; expect fail "indented skill frontmatter with allowed-tools"
D="$(fresh)"; skill_fm 's/$/\r/; s/^name: \(.*\)\r$/name: \1\r\nallowed-tools: Bash\r/'; expect fail "CRLF skill with allowed-tools"
D="$(fresh)"; skill_fm '1s/^/\xef\xbb\xbf/; s/^name: fogar-secure-rust$/name: fogar-secure-rust\nhooks: x/'; expect fail "BOM skill with hooks"
D="$(fresh)"; skill_fm '1s/^---$/--- /; s/^name: fogar-secure-rust$/name: fogar-secure-rust\nallowed-tools: Bash/'; expect fail "'--- ' (trailing space) skill with allowed-tools"
D="$(fresh)"; skill_fm 's/^description: /description:\n  /'; expect fail "skill description continued without a block indicator"
D="$(fresh)"; skill_fm 's/^name: fogar-secure-rust$/name: fogar-secure-rust\nname: other/'; expect fail "duplicate name key in a skill"

[ "$fail" -eq 0 ] && echo "check_vendored self-test: ok"
exit "$fail"
