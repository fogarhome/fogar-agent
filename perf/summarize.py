#!/usr/bin/env python3
"""summarize.py RUNDIR...: one flat JSON per run, keys named as idle-gate.sh reads them.

Exact counters come from proc_pid_rusage (r0/r1 at window start/end); top gives POWER and
per-30 s IDLEW samples (its first sample is cumulative and is dropped)."""
import json, re, statistics, sys

def kv(p):
    return {k: float(v) for k, v in (l.strip().split("=", 1) for l in open(p) if "=" in l)}

for d in sys.argv[1:]:
    a, b = kv(f"{d}/r0.txt"), kv(f"{d}/r1.txt")
    dt = b["wall"] - a["wall"]
    delta = lambda k: b[k] - a[k]
    cpu_ns = delta("user_time_ns") + delta("system_time_ns")
    rows = [l.split() for l in open(f"{d}/top.txt") if re.match(r"\s*\d+\s+\S", l) and "PID" not in l][1:]
    mb = lambda x: round(x / 1048576, 2)
    print(json.dumps({
        "run": d.rstrip("/").split("/")[-1], "window_s": round(dt),
        "cpu_pct_mean": round(100 * cpu_ns / 1e9 / dt, 4),
        "ri_pkg_idle_wkups_per_s": round(delta("pkg_idle_wkups") / dt, 3),
        "ri_interrupt_wkups_per_s": round(delta("interrupt_wkups") / dt, 3),
        "idlew_per_s_top": round(statistics.median(float(r[3]) for r in rows) / 30, 3),
        "power_top_median": statistics.median(float(r[4]) for r in rows),
        "power_top_max": max(float(r[4]) for r in rows),
        "footprint_start_mb": mb(a["phys_footprint"]), "footprint_end_mb": mb(b["phys_footprint"]),
        "footprint_lifetime_max_mb": mb(b["lifetime_max_phys_footprint"]),
        "threads_max": int(open(f"{d}/threads.txt").read()),
        "billed_energy_nj_per_s": round(delta("billed_energy") / dt, 1),
        "cpu_ms_by_qos": {q: round(delta(f"cpu_time_qos_{q}_ns") / 1e6, 2)
                          for q in ("default", "utility", "background")},
        "cpu_ms_total": round(cpu_ns / 1e6, 2),
    }))
