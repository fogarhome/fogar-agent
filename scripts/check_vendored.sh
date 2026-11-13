#!/usr/bin/env bash
# Integrity + hygiene of the agent-team configuration (review sec#6; secskills/REPORT.md).
# Fails if:
#  - anything in the repo is executable agent configuration: a `hooks/` dir, hooks.json,
#    mcp.json / .mcp.json, settings*.json (all case-insensitive, anywhere), or any file under
#    .claude/commands/;
#  - vendored skills or agent definitions differ from their hash lists (agent-team/VENDORED.sha256,
#    agent-team/AGENTS.sha256), files were added/removed, or the root hash of both lists differs
#    from the one recorded in docs/adr/0007-agent-config-integrity.md;
#  - a skill's frontmatter has any key but name/description (so no allowed-tools, hooks, ...);
#  - an agent definition has a key outside the allowlist, a permissionMode other than
#    default/plan, a tool outside the allowlist (`*`, WebFetch, MCP tools, ...), unscoped Bash
#    without a `# unrestricted-bash: <reason>` line, more than 2 skills, or does not disallow the
#    Skill tool; reviewers (`*-reviewer`) get read-only tools + scoped cargo test/clippy + git
#    read commands only.
# LIMIT (also in ADR-0007): the hash lists and the ADR live in the same repo, so a PR can
# regenerate all of them; CI must compare the root hash against the value on the main branch.
set -uo pipefail
cd "$(dirname "$0")/.."
bad=0
say() { echo "FORBIDDEN: $*"; bad=1; }

# --- 1. executable agent configuration anywhere (case-insensitive) -----------------------------
while IFS= read -r -d '' f; do say "$f"; done < <(find . \( -path ./target -o -path ./.git \) -prune -o \
  \( -iname '.mcp.json' -o -iname 'mcp.json' -o -iname 'hooks.json' -o -iname 'settings*.json' \
     -o \( -type d -iname hooks \) \) -print0)
while IFS= read -r -d '' f; do say "command file $f"; done < <(find . \( -path ./target -o -path ./.git \) -prune -o \
  -ipath '*/.claude/commands/*' -print0)

# --- 2. hash lists + recorded root hash --------------------------------------------------------
check_list() { # dir, list (path relative to repo root), find-args...
  local dir="$1" sums="$2"; shift 2
  if [ ! -f "$sums" ]; then say "missing $sums"; return; fi
  local abs; abs="$(realpath "$sums")"
  (cd "$dir" && sha256sum --quiet --strict -c "$abs") || say "content changed in $dir (vs $sums)"
  local listed actual
  listed=$(grep -c . "$sums"); actual=$(cd "$dir" && find . -type f "$@" | wc -l)
  [ "$listed" -eq "$actual" ] || say "files added/removed in $dir ($actual vs $listed hashed)"
}
check_list agent-team/skills agent-team/VENDORED.sha256
check_list agent-team/agents agent-team/AGENTS.sha256 -name '*.md'
if [ -n "$(ls -A .claude/skills 2>/dev/null)" ]; then check_list .claude/skills .claude/skills/VENDORED.sha256 ! -name 'VENDORED.*'; fi
if [ -n "$(ls -A .claude/agents 2>/dev/null)" ]; then check_list .claude/agents .claude/agents/AGENTS.sha256 -name '*.md'; fi
ADR=docs/adr/0007-agent-config-integrity.md
root="$(cat agent-team/VENDORED.sha256 agent-team/AGENTS.sha256 2>/dev/null | sha256sum | cut -d' ' -f1)"
recorded="$(sed -n 's/^ROOT_HASH: *\([0-9a-f]*\).*/\1/p' "$ADR" 2>/dev/null | head -1)"
[ "$root" = "$recorded" ] || say "agent-team root hash $root != ROOT_HASH recorded in $ADR (${recorded:-none})"

# --- 3. frontmatter allowlists -----------------------------------------------------------------
frontmatter() { awk 'NR==1 && $0!="---" {exit} NR>1 && $0=="---" {exit} NR>1 {print}' "$1"; }
# Top-level keys, lower-cased, quotes stripped. A top-level line that is not `key:` is an error.
keys() {
  frontmatter "$1" | awk '
    /^[ \t]/ || /^$/ || /^#/ {next}
    { line=$0
      if (match(line, /^["'"'"']?[A-Za-z0-9_-]+["'"'"']?[ \t]*:/)) {
        k=substr(line, 1, RLENGTH); sub(/[ \t]*:$/, "", k); gsub(/["'"'"']/, "", k); print tolower(k)
      } else { print "<unparseable:" line ">" } }'
}
value() { frontmatter "$1" | sed -n "s/^[\"']\{0,1\}$2[\"']\{0,1\}[[:space:]]*:[[:space:]]*//Ip" | head -1; }

for f in agent-team/skills/*/SKILL.md .claude/skills/*/SKILL.md; do
  [ -f "$f" ] || continue
  for k in $(keys "$f"); do
    case "$k" in name|description) ;; *) say "$f: frontmatter key '$k' (skills may only have name/description)";; esac
  done
done

READONLY_REVIEWER='Read Grep Glob Bash(cargo test:*) Bash(cargo clippy:*) Bash(git diff:*) Bash(git log:*) Bash(git show:*)'
for f in agent-team/agents/*.md .claude/agents/*.md; do
  [ -f "$f" ] || continue
  for k in $(keys "$f"); do
    case "$k" in
      name|description|model|tools|disallowedtools|skills|maxturns|isolation|permissionmode) ;;
      *) say "$f: frontmatter key '$k' not allowed";;
    esac
  done
  pm="$(value "$f" permissionMode)"
  case "${pm//[\"\' ]/}" in ''|default|plan) ;; *) say "$f: permissionMode '$pm' (only default/plan)";; esac
  [ "$(value "$f" disallowedTools)" = "Skill" ] || say "$f does not disallow the Skill tool"
  n=$(value "$f" skills | tr -d '[]' | tr ',' '\n' | grep -c '[a-z]' || true)
  [ "$n" -le 2 ] || say "$f preloads $n skills (max 2)"
  name="$(value "$f" name)"
  tools="$(value "$f" tools)"
  [ -n "$tools" ] || say "$f: no explicit tools list (would inherit everything)"
  # Split on commas that are not inside parentheses.
  mapfile -t toks < <(printf '%s' "$tools" | awk '{
      d=0; t=""
      for (i=1;i<=length($0);i++){c=substr($0,i,1)
        if(c=="(")d++; if(c==")")d--
        if(c=="," && d==0){print t; t=""} else t=t c}
      print t}' | sed 's/^[[:space:]]*//; s/[[:space:]]*$//; s/^["'"'"']//; s/["'"'"']$//')
  for t in "${toks[@]}"; do
    [ -n "$t" ] || continue
    if [[ "$name" == *-reviewer ]]; then
      [[ " $READONLY_REVIEWER " == *" $t "* ]] || say "$f: reviewer tool '$t' (reviewers: read-only + cargo test/clippy + git read)"
      continue
    fi
    case "$t" in
      Read|Grep|Glob|Edit|Write) ;;
      Bash) grep -qE '^# unrestricted-bash: .{10,}' <(frontmatter "$f") \
              || say "$f: unrestricted Bash without a '# unrestricted-bash: <reason>' line";;
      'Bash('*':*)') [[ "$t" != *'*'*':*)' ]] || say "$f: wildcard Bash scope '$t'";;
      *) say "$f: tool '$t' not in the allowlist";;
    esac
  done
done

[ "$bad" -eq 0 ] && echo "vendored hygiene: ok"
exit "$bad"
