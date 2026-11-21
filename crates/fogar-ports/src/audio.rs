//! Audio ports: microphone (`AudioSource`) and speaker (`AudioSink`).
//!
//! Shape decisions (ADR-0006):
//! - The real-time side (OS audio callback, SPSC ring, resampling to 16 kHz mono) lives INSIDE the
//!   adapter. The core receives owned chunks of mono f32 through a bounded [`EventStream`]
//!   (`try_send`, drop-newest-and-count): the adapter never blocks its audio thread.
//! - The speaker takes PCM16 mono through a **non-blocking** `write` that accepts what fits in
//!   the adapter's own bounded buffer and reports the rest as dropped. The core never waits on
//!   the sink, so no polling and no timers are needed for playback.
//! - Sync, dyn-compatible traits: they are opened once per capture/turn, not per sample.
//! - `drop(ActiveCapture)` stops capture; `drop(ActivePlayback)` closes the output device.
//!
//! How Windows/Linux would do this: cpal over WASAPI / ALSA-PipeWire behind the same adapter
//! shape (OS thread + ring + chunker); permission is a no-op `Granted` on Linux and the
//! privacy toggle on Windows. No port change expected.

use crate::{Support, events::EventStream};
use std::{fmt, num::NonZeroUsize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioFormat {
    pub sample_rate_hz: u32,
    pub channels: u16,
}

impl AudioFormat {
    pub const fn mono(sample_rate_hz: u32) -> Self {
        Self {
            sample_rate_hz,
            channels: 1,
        }
    }
}

/// Microphone permission as the OS reports it. Querying it must never prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum MicPermission {
    Granted,
    Denied,
    /// Also the default: "not known yet" fails closed (`open` refuses it).
    #[default]
    NotDetermined,
    Restricted,
    /// ONLY for a platform with no permission model (Linux), where `open` may proceed. An
    /// adapter on a platform that has one (`permission_query` available) maps any status it
    /// cannot read to `NotDetermined`, never to `Unknown`: `Unknown` lets `open` touch the
    /// device, which on macOS would make the OS prompt behind the core's consent point.
    Unknown,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AudioError {
    #[error("not supported by this adapter: {0}")]
    Unsupported(&'static str),
    #[error("permission denied")]
    PermissionDenied,
    #[error("no device")]
    NoDevice,
    #[error("device busy")]
    DeviceBusy,
    #[error("format not supported: {0:?}")]
    FormatUnsupported(AudioFormat),
    #[error("audio backend failure")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// One chunk of captured audio: mono f32 in [-1, 1] at the capture's `format().sample_rate_hz`.
#[derive(Clone, PartialEq)]
pub struct AudioChunk {
    pub samples: Vec<f32>,
}

/// Never print captured audio (fogar-secure-rust CAP/LOG): only its size.
impl fmt::Debug for AudioChunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AudioChunk({} samples)", self.samples.len())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AudioSourceCaps {
    pub capture: Support,
    pub permission_query: Support,
    /// The adapter can ask the OS for the microphone permission (macOS TCC prompt).
    pub permission_request: Support,
}

impl AudioSourceCaps {
    pub const NONE: Self = Self {
        capture: Support::Unsupported,
        permission_query: Support::Unsupported,
        permission_request: Support::Unsupported,
    };
}

/// A running capture. Dropping it stops the device and closes the chunk stream.
pub trait ActiveCapture: Send + fmt::Debug {
    /// Format of the chunks delivered on the stream (mono). Stable for the capture's lifetime.
    fn format(&self) -> AudioFormat;
}

pub trait AudioSource: Send + Sync + 'static {
    /// Stable for the adapter's lifetime. Cheap.
    fn capabilities(&self) -> AudioSourceCaps;
    /// Never prompts. `Unknown` when the platform has no permission model.
    fn mic_permission(&self) -> MicPermission;
    /// Starts capture. Chunks arrive on the returned bounded stream (`capacity` chunks). When the
    /// core lags, the adapter drops the newest chunk and counts it (`EventStream::dropped`).
    /// Errors: `Unsupported` if `capture` is Unsupported, `NoDevice`, ... and `PermissionDenied`
    /// whenever `mic_permission()` is `Denied`, `Restricted` or `NotDetermined`, WITHOUT touching
    /// the device: opening the device with an undecided permission would make the OS prompt on
    /// its own, bypassing the core's single consent point (spec S2). Only `Granted`, and
    /// `Unknown` on an adapter whose `permission_query` is Unsupported (no permission model),
    /// may open. The adapter never prompts from its constructor, `capabilities()`,
    /// `mic_permission()` or anything but `request_mic_permission`.
    fn open(
        &self,
        capacity: NonZeroUsize,
    ) -> Result<(Box<dyn ActiveCapture>, EventStream<AudioChunk>), AudioError>;
    /// Asks the OS for the microphone permission (may show the system prompt). Never blocks:
    /// exactly one `MicPermission` arrives on the returned stream, after which the adapter drops
    /// its end (the stream then yields `None`). When the permission is already decided it is
    /// delivered at once without prompting. Dropping the stream before the answer is safe.
    /// Only the core's capture gate (`fogar-core/src/capture.rs`) calls it, and only on the
    /// person's explicit `RequestMicPermission` (archtest R13; no other crate may call it).
    /// Errors: `Unsupported` when `permission_request` is Unsupported.
    fn request_mic_permission(&self) -> Result<EventStream<MicPermission>, AudioError> {
        Err(AudioError::Unsupported("permission request"))
    }
}

const _: Option<&dyn AudioSource> = None;
const _: Option<&dyn ActiveCapture> = None;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AudioSinkCaps {
    pub playback: Support,
}

impl AudioSinkCaps {
    pub const NONE: Self = Self {
        playback: Support::Unsupported,
    };
}

/// An open output stream. Dropping it closes the device (release it when idle: energy).
pub trait ActivePlayback: Send + fmt::Debug {
    fn format(&self) -> AudioFormat;
    /// Non-blocking. Queues as many samples as fit into the adapter's bounded buffer and returns
    /// how many were accepted; the caller counts the rest as dropped. Never waits.
    fn write(&mut self, pcm: &[i16]) -> usize;
    /// Barge-in: discard everything queued and go silent immediately.
    fn stop(&mut self);
    /// Samples queued but not yet played (used to decide when to close an idle device).
    fn queued_samples(&self) -> usize;
}

pub trait AudioSink: Send + Sync + 'static {
    fn capabilities(&self) -> AudioSinkCaps;
    /// Opens the output at `format` (slice 0 always asks for 24 kHz mono, the TTS format).
    fn open(&self, format: AudioFormat) -> Result<Box<dyn ActivePlayback>, AudioError>;
}

const _: Option<&dyn AudioSink> = None;
const _: Option<&dyn ActivePlayback> = None;
