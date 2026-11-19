#!/usr/bin/env bash
# Mechanical quality gates (no human reviewer: these + independent agent reviewers carry quality).
# Runs every gate in order, never stops early, prints an honest summary: PASS / FAIL / NOT RUN.
# Exit status: 0 only if no gate FAILED. A NOT RUN gate is reported, never counted as passed.
set -uo pipefail
cd "$(dirname "$0")/.."

PINNED="1.97.0"
declare -a NAMES=() RESULTS=() NOTES=()
record() { NAMES+=("$1"); RESULTS+=("$2"); NOTES+=("${3:-}"); }
run_gate() { # name, command...
  local name="$1"; shift
  echo; echo "=== gate: ${name}"; echo "+ $*"
  if "$@"; then record "$name" PASS; else record "$name" FAIL; fi
}

# --- toolchain pin (rust-toolchain.toml). Offline sandboxes may not be able to download the pinned
# channel; fall back to an installed `stable` ONLY if it is exactly the pinned version.
if ! rustup run "$PINNED" rustc --version >/dev/null 2>&1; then
  if rustup run stable rustc --version 2>/dev/null | grep -q "^rustc ${PINNED} "; then
    export RUSTUP_TOOLCHAIN=stable
    record "toolchain ${PINNED}" PASS "pinned channel not installed; using installed stable == ${PINNED}"
  else
    record "toolchain ${PINNED}" FAIL "neither ${PINNED} nor an equal stable toolchain is installed"
  fi
else
  record "toolchain ${PINNED}" PASS
fi
echo "rustc: $(rustc --version)"

run_gate "fmt --check"            cargo fmt --all -- --check
run_gate "clippy -D warnings"     cargo clippy --workspace --all-targets --locked -- -D warnings
run_gate "test --workspace"       cargo test --workspace --locked
run_gate "proto no_std build"     cargo build -p fogar-proto --no-default-features --locked
run_gate "archtest"               cargo test -p fogar-archtest --locked
# fogar-core alone: no other crate's tokio features leak in, so with archtest R10 (no `time`
# feature) any `tokio::time` in core is a compile error.
run_gate "core alone (no tokio::time)" cargo check -p fogar-core --lib --locked
run_gate "vendored skills hygiene" scripts/check_vendored.sh
run_gate "vendored hygiene self-test" scripts/test_check_vendored.sh

if cargo deny --version >/dev/null 2>&1; then
  run_gate "cargo deny check"     cargo deny --locked check
else
  record "cargo deny check" "NOT RUN" "cargo-deny not installed (cargo install --locked cargo-deny --version 0.20.2)"
fi

if cargo audit --version >/dev/null 2>&1; then
  run_gate "cargo audit"          cargo audit
else
  record "cargo audit" "NOT RUN" "cargo-audit not installed (cargo install --locked cargo-audit --version 0.23.0)"
fi

# --- supply chain: cargo vet (supply-chain/, no fetching) and the unsafe count (scripts/geiger.sh).
if cargo vet --version >/dev/null 2>&1; then
  run_gate "cargo vet"            cargo vet --locked
else
  record "cargo vet" "NOT RUN" "cargo-vet not installed (cargo install --locked cargo-vet --version 0.10.3)"
fi
run_gate "geiger check self-test" python3 -I scripts/geiger_check.py --self-test
if cargo geiger --version >/dev/null 2>&1; then
  run_gate "cargo geiger (baseline)" scripts/geiger.sh
else
  record "cargo geiger (baseline)" "NOT RUN" "cargo-geiger not installed (cargo install --locked cargo-geiger --version 0.14.0)"
fi

# --- Miri on every crate compiled on this host that contains `unsafe` (macOS adapters are
# cfg-gated out on Linux and need a macOS runner).
UNSAFE_CRATES=()
for d in crates/*/; do
  c="$(basename "$d")"
  [ "$c" = "fogar-agent-macos" ] && continue
  if grep -rqE '^\s*(unsafe |.*[^/]unsafe \{|unsafe impl)' "$d/src" 2>/dev/null; then UNSAFE_CRATES+=("$c"); fi
done
echo; echo "=== crates with unsafe compiled here: ${UNSAFE_CRATES[*]:-none}"
if [ "${#UNSAFE_CRATES[@]}" -eq 0 ]; then
  record "miri" PASS "no crate with unsafe is compiled on this host"
elif cargo "+${MIRI_TOOLCHAIN:-nightly}" miri --version >/dev/null 2>&1; then
  ok=PASS
  for c in "${UNSAFE_CRATES[@]}"; do
    echo "+ cargo +${MIRI_TOOLCHAIN:-nightly} miri test -p $c"
    env -u RUSTUP_TOOLCHAIN cargo "+${MIRI_TOOLCHAIN:-nightly}" miri test -p "$c" || ok=FAIL
  done
  record "miri (${UNSAFE_CRATES[*]})" "$ok"
else
  record "miri (${UNSAFE_CRATES[*]})" "NOT RUN" "nightly + miri not installable here (static.rust-lang.org unreachable)"
fi

# --- loom: the custom primitive (fogar-core heartbeat). Separate target dir: --cfg changes hashes.
# Review code#8: a filter that matches zero tests must FAIL the gate, not pass it silently.
LOOM_FILTER="${LOOM_FILTER:-heartbeat}"
loom_gate() {
  local out rc n
  out="$(env RUSTFLAGS="--cfg fogar_loom" CARGO_TARGET_DIR=target/loom \
    cargo test -p fogar-core --release --lib --locked "$LOOM_FILTER" 2>&1)"; rc=$?
  echo "$out"
  [ "$rc" -eq 0 ] || return 1
  n="$(echo "$out" | sed -n 's/^test result: ok\. \([0-9]*\) passed.*/\1/p' | awk '{s+=$1} END {print s+0}')"
  if [ "$n" -eq 0 ]; then echo "loom gate: filter '$LOOM_FILTER' matched zero tests"; return 1; fi
  echo "loom gate: $n loom test(s) ran"
}
run_gate "loom (heartbeat)" loom_gate

echo
echo "================ GATES SUMMARY ================"
fail=0
for i in "${!NAMES[@]}"; do
  printf '%-34s %-8s %s\n' "${NAMES[$i]}" "${RESULTS[$i]}" "${NOTES[$i]}"
  [ "${RESULTS[$i]}" = FAIL ] && fail=1
done
echo "==============================================="
[ "$fail" -eq 0 ] && echo "RESULT: no gate failed (NOT RUN gates are listed above)" || echo "RESULT: FAILED"
exit "$fail"
