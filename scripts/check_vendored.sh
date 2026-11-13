#!/usr/bin/env bash
# Integrity + hygiene of the agent-team configuration (review sec#6, round-2 F1/F2; ADR-0007).
# Fails if:
#  - anything in the repo (hidden directories included) is executable agent configuration: a
#    `hooks/` dir, hooks.json, mcp.json / .mcp.json, settings*.json (case-insensitive);
#  - any symlink, FIFO, socket or device file exists anywhere in the repo (none are expected; a
#    symlink would let a file escape the hash lists, which only see regular files);
#  - any `.claude*` file or directory (`.claude-plugin`, `.claude.json`, `sub/.claude/`, ...)
#    exists anywhere except the root `.claude/`, and the root `.claude/` holds anything but
#    `agents/` and `skills/` (so no rules/, commands/, CLAUDE.md, settings, ...);
#  - `.claude/agents` / `.claude/skills` are not byte-identical to `agent-team/agents` /
#    `agent-team/skills` (they may be empty/absent; install_agent_team.sh creates them);
#  - vendored skills or agent definitions differ from their hash lists (agent-team/VENDORED.sha256,
#    agent-team/AGENTS.sha256) — the listed path set must equal the file set exactly — or the root
#    hash of both lists differs from the one recorded in docs/adr/0007-agent-config-integrity.md;
#  - the strict frontmatter parser (scripts/check_frontmatter.py) rejects a skill or agent.
# LIMIT (also in ADR-0007): the hash lists and the ADR live in the same repo, so a PR can
# regenerate all of them; CI must compare the root hash against the value on the main branch.
set -uo pipefail
cd "$(dirname "$0")/.."
bad=0
say() { echo "FORBIDDEN: $*"; bad=1; }
PRUNE=( \( -path ./target -o -path ./.git \) -prune -o )

# --- 1. executable agent configuration anywhere (case-insensitive, hidden dirs included) --------
while IFS= read -r -d '' f; do say "$f"; done < <(find . "${PRUNE[@]}" \
  \( -iname '.mcp.json' -o -iname 'mcp.json' -o -iname 'hooks.json' -o -iname 'settings*.json' \
     -o \( -type d -iname hooks \) \) -print0)

# --- 2. no symlinks or special files anywhere ----------------------------------------------------
while IFS= read -r -d '' f; do say "symlink/special file $f (none are allowed in the repo)"; done \
  < <(find . "${PRUNE[@]}" ! -type f ! -type d -print0)
if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  # Tracked symlinks (mode 120000) and tracked files under the pruned target/ count too.
  while IFS= read -r f; do say "tracked symlink $f"; done < <(git ls-files -s | awk '$1=="120000"{print $4}')
  while IFS= read -r f; do say "tracked file under target/: $f"; done < <(git ls-files target)
fi

# --- 3. .claude* only at the root, and the root .claude/ only holds agents/ + skills/ -----------
while IFS= read -r -d '' f; do say "agent configuration outside the root .claude/: $f"; done \
  < <(find . "${PRUNE[@]}" -iname '.claude*' ! -path ./.claude -print0)
if [ -e .claude ]; then
  while IFS= read -r -d '' f; do
    case "$f" in ./.claude/agents|./.claude/skills) [ -d "$f" ] || say "$f is not a directory";;
      *) say "only agents/ and skills/ may live in .claude/: $f";; esac
  done < <(find ./.claude -mindepth 1 -maxdepth 1 -print0)
  for sub in agents skills; do
    if [ -n "$(ls -A ".claude/$sub" 2>/dev/null)" ]; then
      diff -r --no-dereference "agent-team/$sub" ".claude/$sub" >/dev/null 2>&1 \
        || say ".claude/$sub is not byte-identical to agent-team/$sub (re-run scripts/install_agent_team.sh)"
    fi
  done
fi

# --- 4. hash lists (exact path set) + recorded root hash ----------------------------------------
check_list() { # dir, list (path relative to repo root), find-args...
  local dir="$1" sums="$2"; shift 2
  if [ ! -f "$sums" ]; then say "missing $sums"; return; fi
  local listed actual
  listed="$(sed 's/^[0-9a-f]\{64\}  //' "$sums" | LC_ALL=C sort)"
  actual="$(cd "$dir" && find . -type f "$@" | sed 's#^\./##' | LC_ALL=C sort)"
  if [ "$listed" != "$actual" ]; then say "file set of $dir differs from $sums (added/removed/renamed)"; return; fi
  grep -qvE '^[0-9a-f]{64}  [A-Za-z0-9._/-]+$' "$sums" && { say "malformed line in $sums"; return; }
  local abs; abs="$(realpath "$sums")"
  (cd "$dir" && sha256sum --quiet --strict -c "$abs") || say "content changed in $dir (vs $sums)"
}
check_list agent-team/skills agent-team/VENDORED.sha256 ! -name 'VENDORED.*'
check_list agent-team/agents agent-team/AGENTS.sha256
ADR=docs/adr/0007-agent-config-integrity.md
root="$(cat agent-team/VENDORED.sha256 agent-team/AGENTS.sha256 2>/dev/null | sha256sum | cut -d' ' -f1)"
recorded="$(sed -n 's/^ROOT_HASH: *\([0-9a-f]*\).*/\1/p' "$ADR" 2>/dev/null | head -1)"
[ "$root" = "$recorded" ] || say "agent-team root hash $root != ROOT_HASH recorded in $ADR (${recorded:-none})"

# --- 5. strict frontmatter parser ----------------------------------------------------------------
if command -v python3 >/dev/null 2>&1; then
  python3 -I scripts/check_frontmatter.py . || bad=1
else
  say "python3 not found: the frontmatter checker (scripts/check_frontmatter.py) cannot run"
fi

[ "$bad" -eq 0 ] && echo "vendored hygiene: ok"
exit "$bad"
