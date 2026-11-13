#!/usr/bin/env bash
# Fails if any vendored skill / agent directory (or the repo itself) carries executable agent
# configuration: hooks/, .mcp.json, settings*.json, or broad allowed-tools grants. Skills are inert
# text here; anything that can run on its own must not enter the repo (secskills/REPORT.md).
set -euo pipefail
cd "$(dirname "$0")/.."
bad=0
while IFS= read -r -d '' f; do
  echo "FORBIDDEN: $f"; bad=1
done < <(find . -path ./target -prune -o -path ./.git -prune -o \
  \( -name '.mcp.json' -o -name 'settings*.json' -o \( -type d -name hooks \) \) -print0)
check_skills() { # dir with VENDORED.sha256 (relative to that dir)
  local dir="$1" sums="$2"
  if grep -rniE '^\s*allowed-tools:' "$dir"; then
    echo "FORBIDDEN: allowed-tools grant in a vendored skill under $dir"; bad=1
  fi
  if [ -f "$sums" ]; then
    local abs; abs="$(realpath "$sums")"
    (cd "$dir" && sha256sum --quiet -c "$abs") || { echo "FORBIDDEN: vendored skill content changed in $dir"; bad=1; }
    local listed actual
    listed=$(wc -l < "$sums"); actual=$(cd "$dir" && find . -type f ! -name 'VENDORED.*' | wc -l)
    [ "$listed" -eq "$actual" ] || { echo "FORBIDDEN: files added/removed in $dir ($actual vs $listed hashed)"; bad=1; }
  else
    echo "MISSING: $sums"; bad=1
  fi
}
[ -d agent-team/skills ] && check_skills agent-team/skills agent-team/VENDORED.sha256
[ -d .claude/skills ] && [ -n "$(ls -A .claude/skills 2>/dev/null)" ] && check_skills .claude/skills .claude/skills/VENDORED.sha256
# Subagent definitions: at most 2 skills, Skill tool disallowed, no hooks/mcp keys.
for f in agent-team/agents/*.md .claude/agents/*.md; do
  [ -f "$f" ] || continue
  grep -q '^disallowedTools: Skill' "$f" || { echo "FORBIDDEN: $f does not disallow the Skill tool"; bad=1; }
  if grep -qE '^(hooks|mcpServers):' "$f"; then echo "FORBIDDEN: $f declares hooks/MCP"; bad=1; fi
  n=$(sed -n 's/^skills: \[\(.*\)\]$/\1/p' "$f" | tr ',' '\n' | grep -c '[a-z]' || true)
  [ "$n" -le 2 ] || { echo "FORBIDDEN: $f preloads $n skills (max 2)"; bad=1; }
done
[ "$bad" -eq 0 ] && echo "vendored hygiene: ok"
exit "$bad"
