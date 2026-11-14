#!/bin/sh
# Regenerates Sources/FogarWire/Wire.swift from the Rust wire types (ADR-0004) with typeshare.
# `scripts/gen-wire.sh --check` fails if the committed file differs from a fresh generation.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
repo=$(cd "$here/../.." && pwd)
out="$here/Sources/FogarWire/Wire.swift"
want="1.13.4"
have=$(typeshare --version 2>/dev/null | awk '{print $2}') || true
[ "$have" = "$want" ] || { echo "typeshare-cli $want required (found: ${have:-none}); cargo install typeshare-cli --version $want --locked" >&2; exit 2; }
tmp=$(mktemp -t wire.XXXXXX)
trap 'rm -f "$tmp"' EXIT
typeshare "$repo/crates/fogar-host/src" --lang=swift --config-file "$here/typeshare.toml" --output-file "$tmp" >/dev/null 2>&1
if [ "${1:-}" = "--check" ]; then
  if cmp -s "$tmp" "$out"; then echo "Wire.swift: up to date"; else echo "Wire.swift is stale: run apps/macos/scripts/gen-wire.sh" >&2; diff "$out" "$tmp" >&2 || true; exit 1; fi
else
  cp "$tmp" "$out" && echo "wrote $out"
fi
