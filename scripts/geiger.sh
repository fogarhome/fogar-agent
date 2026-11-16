#!/usr/bin/env bash
# Unsafe-count gate: cargo geiger over the shipped binary (fogar-agentd and everything it links on
# this OS), compared crate by crate against supply-chain/geiger-<os>.txt (scripts/geiger_check.py).
# `scripts/geiger.sh --write` regenerates this OS's baseline; commit it in the PR that changes it.
set -euo pipefail
cd "$(dirname "$0")/.."

os="$(uname -s | tr '[:upper:]' '[:lower:]')"
report="$(mktemp)"; err="$(mktemp)"
trap 'rm -f "$report" "$err"' EXIT
# Own target dir: geiger cleans and rebuilds the packages it scans.
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}/geiger" \
  cargo geiger --manifest-path "$PWD/crates/fogar-agentd/Cargo.toml" --locked --output-format Json >"$report" 2>"$err" \
  || { grep -v '"\$message_type"' "$err" >&2; exit 1; }
grep -v '"\$message_type"' "$err" >&2 || true # cargo's JSON artifact chatter dropped, warnings kept
python3 -I scripts/geiger_check.py "$report" "supply-chain/geiger-${os}.txt" "$@"
