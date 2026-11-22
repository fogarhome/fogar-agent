//! macOS audio adapters (S2 §D): `MacAudioSource` (microphone + TCC permission) and
//! `MacAudioSink` (speaker), on cpal 0.18.2 / CoreAudio with `rtrb` rings between the real-time
//! callbacks and our own threads. Verified on the macOS CI runner (contract suites; the runner has
//! no microphone, so `NoDevice`/`PermissionDenied` are valid outcomes); behaviour, xruns, energy
//! and TCC still need the M2 (see the PR notes).
//!
//! - `dsp`: OS-free RT writer/reader, counters and the resampler (unit-tested, no `unsafe`).
//! - `permission`: `AVCaptureDevice` authorization status/request (the only `unsafe` here).
//! - `capture` / `playback`: one owned thread per open device, which builds, runs and drops the
//!   cpal stream; dropping the handle joins it.

mod capture;
mod dsp;
mod permission;
mod playback;

use fogar_ports::{
    Support,
    audio::{
        ActiveCapture, ActivePlayback, AudioChunk, AudioError, AudioFormat, AudioSink,
        AudioSinkCaps, AudioSource, AudioSourceCaps, MicPermission,
    },
    events::{EventStream, event_channel},
};
use std::{num::NonZeroUsize, sync::atomic::Ordering};

/// Snapshot of the process-wide audio counters (spec S2 §D: input overruns, output gaps, dropped
/// chunks). Monotonic since process start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct AudioCounters {
    pub input_callbacks: u64,
    /// Device frames lost because the capture ring was full.
    pub input_overrun_frames: u64,
    /// 20 ms chunks dropped because the core's stream was full.
    pub chunks_dropped: u64,
    /// Output callbacks that ran dry in the middle of queued audio (audible gaps).
    pub output_underruns: u64,
    /// Errors reported by the audio device (cpal error callback, any kind).
    pub device_errors: u64,
    /// Captures the adapter ended on its own (device lost, stream invalidated, stalled).
    pub captures_aborted: u64,
}

pub fn audio_counters() -> AudioCounters {
    let c = &dsp::COUNTERS;
    AudioCounters {
        input_callbacks: c.input_callbacks.load(Ordering::Relaxed),
        input_overrun_frames: c.input_overrun_frames.load(Ordering::Relaxed),
        chunks_dropped: c.chunks_dropped.load(Ordering::Relaxed),
        output_underruns: c.output_underruns.load(Ordering::Relaxed),
        device_errors: c.device_errors.load(Ordering::Relaxed),
        captures_aborted: c.captures_aborted.load(Ordering::Relaxed),
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MacAudioSource;

impl AudioSource for MacAudioSource {
    fn capabilities(&self) -> AudioSourceCaps {
        let mut c = AudioSourceCaps::NONE;
        c.capture = Support::Native;
        c.permission_query = Support::Native;
        c.permission_request = Support::Native;
        c
    }

    fn mic_permission(&self) -> MicPermission {
        permission::status()
    }

    fn open(
        &self,
        capacity: NonZeroUsize,
    ) -> Result<(Box<dyn ActiveCapture>, EventStream<AudioChunk>), AudioError> {
        // Port contract: only `Granted` opens. Opening CoreAudio input with an undecided
        // permission would make macOS prompt by itself, behind the core's consent point.
        if permission::status() != MicPermission::Granted {
            return Err(AudioError::PermissionDenied);
        }
        capture::open(capacity)
    }

    fn request_mic_permission(&self) -> Result<EventStream<MicPermission>, AudioError> {
        let (sink, stream) = event_channel(NonZeroUsize::MIN);
        permission::request(sink);
        Ok(stream)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MacAudioSink;

impl AudioSink for MacAudioSink {
    fn capabilities(&self) -> AudioSinkCaps {
        let mut c = AudioSinkCaps::NONE;
        c.playback = Support::Native;
        c
    }

    fn open(&self, format: AudioFormat) -> Result<Box<dyn ActivePlayback>, AudioError> {
        playback::open(format)
    }
}
