#!/usr/bin/env bash
# Regenerates the agent-team hash lists after a REVIEWED re-vendor / agent edit and prints the new
# root hash to record in docs/adr/0007-agent-config-integrity.md (ROOT_HASH line). CI compares the
# root hash against the main branch, so a change here must be visible in review (ADR-0007).
set -euo pipefail
cd "$(dirname "$0")/.."
(cd agent-team/skills && find . -type f ! -name 'VENDORED.*' -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sed 's#  \./#  #') > agent-team/VENDORED.sha256
(cd agent-team/agents && find . -type f -name '*.md' -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sed 's#  \./#  #') > agent-team/AGENTS.sha256
cat agent-team/VENDORED.sha256 agent-team/AGENTS.sha256 | sha256sum | cut -d' ' -f1
