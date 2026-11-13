//! Headless Linux in slice 0 has no audio adapter: both directions say Unsupported explicitly
//! (contract-checked) and the core degrades gracefully. S8: cpal over PipeWire/ALSA.

use fogar_ports::{
    audio::{
        ActiveCapture, ActivePlayback, AudioChunk, AudioError, AudioFormat, AudioSink,
        AudioSinkCaps, AudioSource, AudioSourceCaps, MicPermission,
    },
    events::EventStream,
};
use std::num::NonZeroUsize;

#[derive(Debug, Clone, Copy, Default)]
pub struct NoAudioSource;

impl AudioSource for NoAudioSource {
    fn capabilities(&self) -> AudioSourceCaps {
        AudioSourceCaps::NONE
    }
    fn mic_permission(&self) -> MicPermission {
        MicPermission::Unknown
    }
    fn open(
        &self,
        _capacity: NonZeroUsize,
    ) -> Result<(Box<dyn ActiveCapture>, EventStream<AudioChunk>), AudioError> {
        Err(AudioError::Unsupported(
            "capture (slice 0 Linux build has no audio adapter)",
        ))
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoAudioSink;

impl AudioSink for NoAudioSink {
    fn capabilities(&self) -> AudioSinkCaps {
        AudioSinkCaps::NONE
    }
    fn open(&self, _format: AudioFormat) -> Result<Box<dyn ActivePlayback>, AudioError> {
        Err(AudioError::Unsupported(
            "playback (slice 0 Linux build has no audio adapter)",
        ))
    }
}
