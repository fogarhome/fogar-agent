#!/bin/bash
# =============================================================================
# REFERENCE SCRIPT — NOT AUTO-RUN. NOT WIRED TO ANY HOOK. UNTESTED ON macOS.
# Purpose: per-slice 10-minute idle efficiency gate for fogar-agent on the M2.
# Read references/measurement-and-gate.md first. Copy into the repo (e.g.
# perf/idle-gate.sh), review, adapt, then run manually or from CI on the M2.
# Written for macOS /bin/bash 3.2 (no associative arrays, no mapfile).
#
# Usage:
#   idle-gate.sh --pid <PID> [--out DIR] [--warmup 120] [--duration 600] [--interval 30]
#                [--baseline perf/baselines/<slice>.json] [--meter path/to/fogar-meter]
#   idle-gate.sh --cmd "/path/to/fogar-agent --config prod.toml" [same options]
#
# Optional:
#   --meter: dev helper that prints rusage counters for a pid as
#            "key=value" lines (see measurement-and-gate.md §6). Exact counters.
#   POWERMETRICS=1 env: also run `sudo -n powermetrics` if passwordless sudo works.
#
# Exit: 0 pass, 1 threshold/regression fail, 2 precondition/usage error.
# =============================================================================
set -u

PID=""; CMD=""; OUT="./idle-gate-$(date +%Y%m%d-%H%M%S)"
WARMUP=120; DURATION=600; INTERVAL=30; BASELINE=""; METER=""

# --- working-target thresholds (calibrate; see measurement-and-gate.md §4) ---
MAX_CPU_PCT=${MAX_CPU_PCT:-1.0}
MAX_IDLEW_PER_S=${MAX_IDLEW_PER_S:-2.0}
MAX_INTRW_PER_S=${MAX_INTRW_PER_S:-5.0}
MAX_FOOTPRINT_MB=${MAX_FOOTPRINT_MB:-50}
MAX_SLOPE_MB_PER_H=${MAX_SLOPE_MB_PER_H:-2}
MAX_THREADS=${MAX_THREADS:-16}
REG_RATIO=${REG_RATIO:-1.25}

while [ $# -gt 0 ]; do
  case "$1" in
    --pid) PID="$2"; shift 2;;
    --cmd) CMD="$2"; shift 2;;
    --out) OUT="$2"; shift 2;;
    --warmup) WARMUP="$2"; shift 2;;
    --duration) DURATION="$2"; shift 2;;
    --interval) INTERVAL="$2"; shift 2;;
    --baseline) BASELINE="$2"; shift 2;;
    --meter) METER="$2"; shift 2;;
    *) echo "unknown arg: $1" >&2; exit 2;;
  esac
done

[ "$(uname -s)" = "Darwin" ] || { echo "macOS only" >&2; exit 2; }
mkdir -p "$OUT" || exit 2
log(){ echo "[$(date +%H:%M:%S)] $*" | tee -a "$OUT/gate.log"; }

# --- preconditions (recorded; AC power is enforced) ---
sw_vers > "$OUT/env.txt" 2>&1
sysctl -n hw.model >> "$OUT/env.txt" 2>&1
pmset -g batt >> "$OUT/env.txt" 2>&1
pmset -g therm >> "$OUT/env.txt" 2>&1
if ! pmset -g batt | grep -q "AC Power"; then
  log "PRECONDITION FAIL: not on AC power"; exit 2
fi
top -l 2 -s 5 -o cpu -n 5 -stats pid,command,cpu > "$OUT/background-load.txt" 2>&1

# --- start or attach ---
STARTED=0
if [ -n "$CMD" ]; then
  log "starting: $CMD"
  # shellcheck disable=SC2086
  $CMD > "$OUT/daemon.stdout" 2> "$OUT/daemon.stderr" &
  PID=$!; STARTED=1
fi
[ -n "$PID" ] || { echo "need --pid or --cmd" >&2; exit 2; }
kill -0 "$PID" 2>/dev/null || { log "pid $PID not running"; exit 2; }

caffeinate -i -w "$PID" & CAFF=$!
cleanup(){ kill "$CAFF" 2>/dev/null; [ "$STARTED" = 1 ] && kill "$PID" 2>/dev/null; }
trap cleanup EXIT

log "warm-up ${WARMUP}s"; sleep "$WARMUP"
kill -0 "$PID" 2>/dev/null || { log "daemon died during warm-up"; exit 1; }

# --- exact counters (optional helper) ---
meter_snap(){ [ -n "$METER" ] && "$METER" "$PID" > "$OUT/meter-$1.txt" 2>&1; }
meter_snap start
pmset -g assertions > "$OUT/assertions-start.txt" 2>&1

# --- optional powermetrics (root) ---
if [ "${POWERMETRICS:-0}" = 1 ] && sudo -n true 2>/dev/null; then
  N=$(( DURATION / 60 )); [ "$N" -lt 1 ] && N=1
  sudo -n powermetrics --samplers tasks --show-process-energy -i 60000 -n "$N" \
       -o "$OUT/powermetrics.txt" > /dev/null 2>&1 &
  log "powermetrics running ($N x 60s)"
fi

# --- sampled series via top (first sample has no valid deltas) ---
SAMPLES=$(( DURATION / INTERVAL + 1 ))
log "sampling ${SAMPLES} x ${INTERVAL}s with top"
top -l "$SAMPLES" -s "$INTERVAL" -pid "$PID" \
    -stats pid,cpu,idlew,power,mem,threads,csw > "$OUT/top.txt" 2>&1
meter_snap end
pmset -g assertions > "$OUT/assertions-end.txt" 2>&1
kill -0 "$PID" 2>/dev/null || { log "daemon died during measurement"; exit 1; }

# --- parse: lines starting with the pid; skip first sample ---
# columns: PID %CPU IDLEW POWER MEM #TH CSW ; MEM like 12M / 3456K / 1.1G with optional +/- suffix
awk -v pid="$PID" -v interval="$INTERVAL" '
function mb(s){ gsub(/[+-]$/,"",s); u=substr(s,length(s)); v=substr(s,1,length(s)-1)+0;
  if(u=="K")return v/1024; if(u=="M")return v; if(u=="G")return v*1024; if(u=="B")return v/1048576; return s/1048576 }
$1==pid { n++; if(n==1) next; k++;
  cpu+= $2; if($2>cpumax)cpumax=$2;
  w = $3; gsub(/[+-]$/,"",w); idlew+= w;
  m=mb($5); if(m>mmax)mmax=m; mend=m; t=(k-1)*interval/3600; sx+=t; sy+=m; sxx+=t*t; sxy+=t*m;
  th=$6; gsub(/\/.*/,"",th); th+=0; if(th>thmax)thmax=th }
END { if(k<2){print "parse_error=1"; exit}
  slope=(k*sxy-sx*sy)/(k*sxx-sx*sx);
  printf "samples=%d\ncpu_pct_mean=%.3f\ncpu_pct_max=%.3f\nidlew_per_s_top=%.3f\nfootprint_max_mb=%.2f\nfootprint_end_mb=%.2f\nfootprint_slope_mb_per_h=%.3f\nthreads_max=%d\n",
    k, cpu/k, cpumax, idlew/(k*interval), mmax, mend, slope, thmax }' "$OUT/top.txt" > "$OUT/summary.txt"

# --- helper deltas override top-derived wakeups when available ---
if [ -n "$METER" ] && [ -s "$OUT/meter-start.txt" ] && [ -s "$OUT/meter-end.txt" ]; then
  for key in ri_pkg_idle_wkups ri_interrupt_wkups; do
    a=$(awk -F= -v k=$key '$1==k{print $2}' "$OUT/meter-start.txt")
    b=$(awk -F= -v k=$key '$1==k{print $2}' "$OUT/meter-end.txt")
    [ -n "$a" ] && [ -n "$b" ] && awk -v a="$a" -v b="$b" -v d="$DURATION" -v k="$key" \
        'BEGIN{printf "%s_per_s=%.3f\n", k, (b-a)/d}' >> "$OUT/summary.txt"
  done
fi
cat "$OUT/summary.txt" | tee -a "$OUT/gate.log"

get(){ awk -F= -v k="$1" '$1==k{print $2}' "$OUT/summary.txt" | tail -1; }
IDLEW=$(get ri_pkg_idle_wkups_per_s); [ -z "$IDLEW" ] && IDLEW=$(get idlew_per_s_top)
INTRW=$(get ri_interrupt_wkups_per_s)
FAIL=0
chk(){ # name value limit
  [ -z "$2" ] && { log "SKIP $1 (no data)"; return; }
  if awk -v v="$2" -v l="$3" 'BEGIN{exit !(v>l)}'; then log "FAIL $1=$2 > $3"; FAIL=1; else log "ok   $1=$2 <= $3"; fi; }
chk cpu_pct_mean "$(get cpu_pct_mean)" "$MAX_CPU_PCT"
chk idle_wakeups_per_s "$IDLEW" "$MAX_IDLEW_PER_S"
chk intr_wakeups_per_s "$INTRW" "$MAX_INTRW_PER_S"
chk footprint_end_mb "$(get footprint_end_mb)" "$MAX_FOOTPRINT_MB"
chk footprint_slope_mb_per_h "$(get footprint_slope_mb_per_h)" "$MAX_SLOPE_MB_PER_H"
chk threads_max "$(get threads_max)" "$MAX_THREADS"
if grep -q "pid $PID" "$OUT/assertions-end.txt" 2>/dev/null; then log "FAIL daemon holds a power assertion"; FAIL=1; fi

# --- regression vs baseline (flat JSON: {"cpu_pct_mean":0.02,...}) ---
if [ -n "$BASELINE" ] && [ -f "$BASELINE" ]; then
  for key in cpu_pct_mean footprint_end_mb idlew_per_s_top ri_pkg_idle_wkups_per_s; do
    base=$(sed -n "s/.*\"$key\"[[:space:]]*:[[:space:]]*\([0-9.eE+-]*\).*/\1/p" "$BASELINE" | head -1)
    cur=$(get "$key")
    [ -z "$base" ] || [ -z "$cur" ] && continue
    case $key in cpu_pct_mean) floor=0.05;; footprint_end_mb) floor=2;; *) floor=0.2;; esac
    if awk -v c="$cur" -v b="$base" -v r="$REG_RATIO" -v f="$floor" 'BEGIN{exit !(c>b*r && c>b+f)}'; then
      log "REGRESSION $key: $cur vs baseline $base"; FAIL=1
    fi
  done
fi

# --- result.json (flat) ---
{ echo "{"; sed 's/^\([^=]*\)=\(.*\)$/  "\1": \2,/' "$OUT/summary.txt"; echo "  \"fail\": $FAIL"; echo "}"; } > "$OUT/result.json"
log "result: $([ $FAIL = 0 ] && echo PASS || echo FAIL)  ($OUT/result.json)"
exit $FAIL
