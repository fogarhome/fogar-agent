#!/usr/bin/env bash
# Installs the reviewed agent-team config into .claude/ (skills + subagent definitions).
# Run by a human after reviewing agent-team/. Verifies hashes first; copies no hooks/settings/MCP.
set -euo pipefail
cd "$(dirname "$0")/.."
scripts/check_vendored.sh
# .claude/agents and .claude/skills must be byte-identical copies of agent-team/ (the checker
# diffs them); the hash lists stay in agent-team/ only.
rm -rf .claude/skills .claude/agents
mkdir -p .claude
cp -R agent-team/skills .claude/skills
cp -R agent-team/agents .claude/agents
scripts/check_vendored.sh
echo "installed agent-team into .claude/"
