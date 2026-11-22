//! macOS audio adapters (S2 §D): `MacAudioSource` (microphone + TCC permission) and
//! `MacAudioSink` (speaker), on cpal 0.18.2 / CoreAudio with `rtrb` rings between the real-time
//! callbacks and our own threads. What CI proves: on the macOS runner the contract suites pass,
//! but the microphone path ends in `PermissionDenied`/`NoDevice` there, so capture itself is NOT
//! exercised by CI; only playback runs against a real (virtual) output. Capture, xruns, energy
//! and TCC need the M2 (see the PR notes). The OS-free parts (`crate::rt`) are unit-tested on
//! every host.
//!
//! - `permission`: `AVCaptureDevice` authorization status/request (the only `unsafe` here).
//! - `capture` / `playback`: one owned thread per open device (`rt::worker`), which builds,
//!   runs and drops the cpal stream; dropping the handle stops it within a deadline.

mod capture;
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
    /// Short gaps in playback (ring ran dry and audio resumed within 250 ms): audible xruns.
    pub output_underruns: u64,
    /// Errors reported by the audio device (cpal error callback, any kind).
    pub device_errors: u64,
    /// Captures the adapter ended on its own (device lost, stalled, permission revoked).
    pub captures_aborted: u64,
    /// Resampled output samples the ring had no room for (expected 0).
    pub output_samples_dropped: u64,
    /// `open` refused at once because an earlier timed-out open is still inside CoreAudio.
    pub opens_refused_in_flight: u64,
    /// Audio threads detached because they did not stop within 1 s of their handle's drop.
    pub threads_detached: u64,
}

pub fn audio_counters() -> AudioCounters {
    let c = &crate::rt::dsp::COUNTERS;
    AudioCounters {
        input_callbacks: c.input_callbacks.load(Ordering::Relaxed),
        input_overrun_frames: c.input_overrun_frames.load(Ordering::Relaxed),
        chunks_dropped: c.chunks_dropped.load(Ordering::Relaxed),
        output_underruns: c.output_underruns.load(Ordering::Relaxed),
        device_errors: c.device_errors.load(Ordering::Relaxed),
        captures_aborted: c.captures_aborted.load(Ordering::Relaxed),
        output_samples_dropped: c.output_samples_dropped.load(Ordering::Relaxed),
        opens_refused_in_flight: c.opens_refused_in_flight.load(Ordering::Relaxed),
        threads_detached: c.threads_detached.load(Ordering::Relaxed),
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
        permission::request(sink)?;
        Ok(stream)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MacAudioSink;

impl AudioSink for MacAudioSink {
    fn capabilities(&self) -> AudioSinkCaps {
        let mut c = AudioSinkCaps::NONE;
        c.playback = Support::Native;
        c.health = Support::Native;
        c
    }

    fn open(&self, format: AudioFormat) -> Result<Box<dyn ActivePlayback>, AudioError> {
        playback::open(format)
    }
}
