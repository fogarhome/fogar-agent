#!/usr/bin/env bash
# Installs the reviewed agent-team config into .claude/ (skills + subagent definitions).
# Run by a human after reviewing agent-team/. Verifies hashes first; copies no hooks/settings/MCP.
set -euo pipefail
cd "$(dirname "$0")/.."
scripts/check_vendored.sh
mkdir -p .claude/skills .claude/agents
cp -R agent-team/skills/. .claude/skills/
cp agent-team/VENDORED.md agent-team/VENDORED.sha256 .claude/skills/
cp agent-team/agents/*.md .claude/agents/
scripts/check_vendored.sh
echo "installed agent-team into .claude/"
