#!/usr/bin/env python3
"""Print proc_pid_rusage(RUSAGE_INFO_V4) counters for a pid as key=value (exact, cumulative)."""
import ctypes, sys, time

class RU(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [(n, ctypes.c_uint64) for n in (
        "user_time", "system_time", "pkg_idle_wkups", "interrupt_wkups", "pageins", "wired_size",
        "resident_size", "phys_footprint", "proc_start_abstime", "proc_exit_abstime",
        "child_user_time", "child_system_time", "child_pkg_idle_wkups", "child_interrupt_wkups",
        "child_pageins", "child_elapsed_abstime", "diskio_bytesread", "diskio_byteswritten",
        "cpu_time_qos_default", "cpu_time_qos_maintenance", "cpu_time_qos_background",
        "cpu_time_qos_utility", "cpu_time_qos_legacy", "cpu_time_qos_user_initiated",
        "cpu_time_qos_user_interactive", "billed_system_time", "serviced_system_time",
        "logical_writes", "lifetime_max_phys_footprint", "instructions", "cycles",
        "billed_energy", "serviced_energy", "interval_max_phys_footprint", "runnable_time")]

libc = ctypes.CDLL("/usr/lib/libproc.dylib")
tb = ctypes.CDLL("/usr/lib/libSystem.dylib")
class TB(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]
t = TB(); tb.mach_timebase_info(ctypes.byref(t))
r = RU()
if libc.proc_pid_rusage(int(sys.argv[1]), 4, ctypes.byref(r)) != 0:
    sys.exit("proc_pid_rusage failed")
ns = lambda v: v * t.numer // t.denom  # rusage times are mach absolute units
print(f"wall={time.time():.3f}")
for k in ("user_time", "system_time", "cpu_time_qos_default", "cpu_time_qos_utility",
          "cpu_time_qos_background", "runnable_time"):
    print(f"{k}_ns={ns(getattr(r, k))}")
for k in ("pkg_idle_wkups", "interrupt_wkups", "phys_footprint", "lifetime_max_phys_footprint",
          "billed_energy", "instructions"):
    print(f"{k}={getattr(r, k)}")
