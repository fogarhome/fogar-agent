# Format negotiation and resampling (rubato 5)

## Negotiation policy
1. Take `device.default_input_config()` (cpal 0.18 prefers 48 kHz, then 44.1 kHz, then max) [primary].
   On macOS 0.18.2 the format is F32 or I16 only [primary]; on other hosts it may be I32/I24/F64.
2. Do **not** request 16 kHz from the device: cpal sets the hardware nominal rate
   (`set_physical_format`/`set_sample_rate`) and every rate change invalidates other streams
   (`StreamInvalidated`) [primary]. Use whatever the device runs at.
3. Channels: accept the device count; downmix to mono in `RtSink`.
4. Pipeline internal format: mono f32 at 16 kHz (openWakeWord, earshot, livekit-wakeword and
   rustpotter all work at 16 kHz [primary: their READMEs]). Opus can take 16 kHz directly
   (allowed rates 8000/12000/16000/24000/48000 [primary: opus.h]).
5. If the device already runs at 16 kHz (some USB/BT mics): skip the resampler.

## rubato 5.0.1 (2026-10-01) — API facts [primary: README + src]
- Major versions 1→5 all shipped in 2026: re-check docs.rs before coding.
- Data goes through `audioadapter` traits (`Adapter`/`AdapterMut`); `audioadapter-buffers`
  provides `direct::InterleavedSlice::new(&buf, channels, frames)` / `new_mut`.
- Types: `Fft<T>` (synchronous, fixed ratio, feature `fft_resampler`, default on),
  `Async<T>::new_sinc(..)` / `Async<T>::new_poly(ratio, max_rel, PolynomialDegree, chunk, ch, FixedAsync)`,
  `Slip` (ppm drift only).
- `Fft::<f32>::new(sr_in, sr_out, chunk_size, channels, FixedSync::{Input|Output|Both})`.
- Real-time use: `process_into_buffer(&in_adapter, &mut out_adapter, Option<&Indexing>)` "performs no
  allocations"; create resampler and buffers up front; keep the `log` feature **off** ("outputting a log
  message allocates a String") [primary].
- Always ask `input_frames_next()` / `output_frames_max()`; with `FixedSync::Input` the output count
  varies per call (observed 320/321 for 960 in) [exp]. Startup delay: `output_delay()` = 53 frames for
  48k→16k, chunk 960 [exp].

## Which resampler for 48k/44.1k → 16k
| Case | Choice | Reason |
|---|---|---|
| 48 kHz → 16 kHz (3:1) | `Fft::<f32>::new(48000,16000,960,1,FixedSync::Input)` | fixed ratio, high quality, ~0.05 % of a core [exp] |
| 44.1 kHz → 16 kHz | `Fft` with chunk multiple of 441 (e.g. 882) and `FixedSync::Input` | ratio 160/441 fixed |
| clocks drift / ratio changes | `Async::new_poly` (cheap) | wake word tolerates slight aliasing [inference] |
Clock drift between mic and wall-clock does not matter for a capture-only pipeline (no output device
to stay in sync with) — a fixed-ratio resampler is correct.

## Code (compiled & run on Linux, 0 allocations in steady state) [exp]

```rust
use audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

let chunk = 960; // 20 ms @ 48 kHz
let mut rs = Fft::<f32>::new(48000, 16000, chunk, 1, FixedSync::Input)?;
let mut out = vec![0f32; rs.output_frames_max()];      // allocate once
let cap = out.len();
// loop on the dsp thread, `inp` = 960 mono frames popped from the rtrb consumer
let i = InterleavedSlice::new(&inp, 1, chunk)?;
let mut o = InterleavedSlice::new_mut(&mut out, 1, cap)?;
let (_read, written) = rs.process_into_buffer(&i, &mut o, None)?;
// out[..written] → VAD / wake word / Opus
```
On rebuild (new device/rate): construct a new resampler (expensive; fine off the RT thread) and
reset downstream state.

## Accumulation buffers on the dsp thread
- Pop from rtrb into a fixed `[f32; 960]` accumulator; when full, resample.
- Downstream consumers want different frame sizes: earshot 256, openWakeWord 1280, Opus 320 (20 ms @16k).
  Use one 16 kHz ring/`VecDeque` with preallocated capacity (2 s pre-roll = 32 000 samples) and read
  each consumer's frame size from it. Allocation on the dsp thread is allowed but should be
  steady-state-free to keep the footprint flat.
