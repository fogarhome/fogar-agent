#!/bin/bash
# perf/measure.sh PID OUTDIR [WARMUP=120] [DURATION=600]: idle-gate sampling of one pid.
set -u
PID=$1; OUT=$2; WARMUP=${3:-120}; DUR=${4:-600}
S=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$OUT"
sleep "$WARMUP"
{ date; sysctl -n vm.loadavg; } > "$OUT/load0.txt"
python3 -I "$S/rusage.py" "$PID" > "$OUT/r0.txt"
footprint -p "$PID" > "$OUT/footprint0.txt" 2>&1
top -l $((DUR / 30 + 1)) -s 30 -pid "$PID" -stats pid,command,cpu,idlew,power,th,mem > "$OUT/top.txt" 2>&1
python3 -I "$S/rusage.py" "$PID" > "$OUT/r1.txt"
footprint -p "$PID" > "$OUT/footprint1.txt" 2>&1
ps -M -p "$PID" | tail -n +2 | wc -l | tr -d ' ' > "$OUT/threads.txt"
pmset -g assertions > "$OUT/assertions.txt" 2>&1
{ date; sysctl -n vm.loadavg; } > "$OUT/load1.txt"
echo done > "$OUT/DONE"
