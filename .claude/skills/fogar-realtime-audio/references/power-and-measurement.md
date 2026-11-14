# Power cost of an always-open mic, and the M2 measurement plan

## What is known
- **No primary source gives a number** for the energy cost of keeping the built-in mic + CoreAudio
  input running on Apple Silicon. Everything below is a plan, not a result.
- Fixed costs of an open input stream (all [unverified] magnitudes): the HAL IO thread wakes once per
  buffer period (≈50–100 times/s at 10–20 ms); the codec/mic path stays powered; the menu-bar orange
  dot is permanent [primary for the dot].
- Our own costs, measured on Linux x86 (Xeon 2.1 GHz, single thread, 60 s audio) [exp]:

| Stage | % of one x86 core |
|---|---|
| callback downmix + rtrb push | 0.005 % |
| rubato Fft 48k→16k | 0.045 % |
| earshot VAD | 0.05–0.065 % |
| Opus 16k mono 24 kb/s (only after wake) | 0.30–0.55 % |
| openWakeWord (tract) mel+embedding+1 classifier | 3.2–3.6 % |

  → With VAD gating, the always-on part is ≈0.1 % of an x86 core; ungated wake word dominates at
  ≈3.5 %. On M2 E-cores expect higher % but lower energy per op [unverified]. The working target
  "<1 % CPU at idle" is plausible **only with gating or a cheaper wake model** — measure.

## Measurement plan on the MacBook Air M2 (run each ≥ 30 min, battery, display on & off)

Scenarios:
- A: agent running, capture **disabled** (baseline).
- B: capture open, silence, VAD only (gating on).
- C: capture open, silence, wake word ungated.
- D: capture open, continuous speech/TV (worst case, gating on).
- E: B with `ProcessType=Background` vs unset; B with dsp QoS utility vs background.
- F: buffer request 5 ms vs 10 ms vs 20 ms (`BufferSize::Fixed`).
- G: built-in mic vs AirPods mic (check output sample-rate change in Audio MIDI Setup).

Tools/metrics:
- `sudo powermetrics --samplers tasks,cpu_power -i 5000 --show-process-energy` → per-process
  energy impact, **idle wakeups/s**, CPU ms/s, package power [secondary: tool help; verify flags
  with `powermetrics --help` on the machine].
- Activity Monitor ▸ Energy ("Energy Impact", "12 hr Power") — coarse cross-check.
- `top -l 0 -s 5 -stats pid,command,cpu,mem,power,idlew -pid <pid>` [unverified column names; check
  `man top`].
- In-process counters exported via the agent's status IPC: `callbacks/s`, actual callback size,
  `frames_dropped`, VAD duty %, inference ms per 80 ms, Opus ms per 20 ms, queue depth, rebuild count.
- Footprint: `footprint <pid>` / `vmmap --summary <pid>` at t=0, 1 h, 24 h, 72 h (target <50 MB;
  tract allocates ~350×/frame in our benchmark → check fragmentation growth).
- Instruments: "Time Profiler" + "System Trace" on the dsp thread; RTSan run for the callback.

Robustness soak (72 h, scripted by a human, not by the agent):
- Every 10 min toggle default input (built-in ↔ USB mic) — expect rebuild < 2 s, no leak.
- AirPods connect/disconnect; USB mic unplug while streaming.
- 20 sleep/wake cycles (lid close/open; `pmset sleepnow`) — heartbeat recovery time.
- Revoke microphone permission in System Settings while running — does input become zeros, does the
  silence detector fire, does anything crash?
- CPU saturation (`yes > /dev/null` × 8) — `frames_dropped` must stay 0 with a 3 s ring.
- Wake-word quality: false accepts per hour on 8 h of household audio (TV, music, conversation);
  false rejects on 100 utterances × 3 speakers × 2 distances, gating on/off.

Accept criteria (proposed, adjust after first measurements):
- Scenario B: < 1 % CPU of one core averaged, < 150 extra idle wakeups/s vs A, energy impact in the
  "low" band; RSS < 50 MB.
- Zero `frames_dropped` in B–E under normal load; recovery from any device event < 3 s.
