# Thermal, memory-pressure and power signals on macOS → load shedding

The MacBook Air M2 is fanless: sustained load raises temperature, the system throttles, and everything the
user runs slows down. The daemon must notice and back off *before* that, without polling.
All macOS behaviour **untested**.

## 1. Signal sources (event-driven, zero idle wakeups)

| Signal | Event-driven source | Value read | Status |
|---|---|---|---|
| Thermal | notify(3) name `kOSThermalNotificationPressureLevelName` (Libc `OSThermalNotification.h`, macOS 10.10+); register with `notify_register_file_descriptor` → fd usable with tokio `AsyncFd`; read level with `notify_get_state` | `OSThermalPressureLevel`: macOS values `Nominal=0, Moderate, Heavy, Trapping, Sleeping` | [primary headers]; the string value of the name is resolved at link time (extern const) |
| Thermal (Foundation) | `NSProcessInfo.thermalState` + `NSProcessInfoThermalStateDidChangeNotification` (objc2-foundation 0.3.2: `thermalState()`, `NSProcessInfoThermalState::{Nominal,Fair,Serious,Critical}`) | 0..3 | [primary bindings]; notification delivery needs a queue/run loop — reading `thermalState()` on each work-batch start is a fine fallback |
| Memory pressure | `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE`, mask `NORMAL|WARN|CRITICAL` (libdispatch `source.h`; dispatch2 0.3.1 `DispatchSource::new` (unsafe) + `dispatch_source_memorypressure_flags_t`) | flags in `data()` | [primary]; handler runs on a GCD queue → forward into tokio with a `watch` sender |
| Power source (AC/battery) | notify(3) `kIOPSNotifyPowerSource` = `"com.apple.system.powersources.source"`; also `kIOPSNotifyLowBattery`, `kIOPSNotifyTimeRemaining` (IOKit `IOPowerSources.h`) then query `IOPSGetTimeRemainingEstimate` / providing power source | AC vs battery, time remaining | [primary header] |
| Low Power Mode | `NSProcessInfo.isLowPowerModeEnabled` + `NSProcessInfoPowerStateDidChangeNotification` | bool | [primary bindings]; macOS availability version [unverified] |
| Low disk | notify(3) `kNotifyVFSLowDiskSpace` = `"com.apple.system.lowdiskspace"` (Libnotify `notify_keys.h`) | — | [primary] |

notify(3) fd delivery: "Notifications are delivered by an integer value written to the file descriptor. The value
will match the notification token" (notify.h). One fd can carry several names (`NOTIFY_REUSE`); service it
promptly ("the kernel limits the buffer space for queued writes"). The `libc` crate does **not** declare
`notify_*`; declare the externs yourself (they are in libSystem) — check signatures against `notify.h`.

## 2. The state the core sees

```rust
// core (OS-agnostic) — plain data, no OS types
pub enum Thermal { Nominal, Elevated, Serious, Critical }
pub enum MemPressure { Normal, Warn, Critical }
pub enum Power { Ac, Battery { low: bool }, Unknown }
pub struct PowerState { pub thermal: Thermal, pub mem: MemPressure, pub power: Power, pub low_power_mode: bool }
```
macOS mapping (proposal, validate): `OSThermalPressureLevel` Nominal→Nominal, Moderate→Elevated,
Heavy→Serious, Trapping|Sleeping→Critical; `NSProcessInfoThermalState` Fair→Elevated, Serious→Serious,
Critical→Critical.

## 3. Shedding policy (pure function in core: `fn budget(&PowerState) -> Budget`)

| Condition | Budget effect |
|---|---|
| all nominal, AC | normal: batch jobs allowed (still at BG QoS), reconnect cap 5 min, keepalive per server need |
| Battery or Low Power Mode | defer non-urgent batch work until AC or a daily window; double heartbeat/keepalive interval where protocol allows; no prefetch |
| Thermal Elevated | pause batch work; cap concurrency of CPU work to 1; stretch debounce windows ×2 |
| Thermal Serious/Critical | only latency-critical protocol duties; refuse new heavy jobs with a typed `Overloaded` error; drop optional telemetry |
| Mem Warn | lower cache caps for *new* entries, stop prefetch, call allocator release once (do **not** walk & drop existing caches — Apple header guidance) |
| Mem Critical | as Warn + reject large requests, shrink buffers on next reuse |
| back to normal | restore with hysteresis (e.g. stay degraded ≥ 60 s after last elevated signal) to avoid flapping |

Rules: shedding decisions are made **at work admission**, not by preempting running work; every shed is logged
once per transition (not per item) and counted in metrics.

## 4. Testing the reactions

* Memory: `memory_pressure -S -l warn` / `-l critical` (system_cmds `memory_pressure(1)`: "-S Simulate memory
  pressure ... at the warn or critical level") [primary].
* Thermal: no CLI simulator found in OSS. Make the adapter injectable (port has a test double) and unit-test the
  core policy; on the M2 run a sustained CPU load (e.g. `yes > /dev/null` ×8 for minutes) and log transitions
  [untested].
* Power: unplug AC during the soak test; toggle Low Power Mode in System Settings.
