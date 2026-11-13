//! UNTESTED STUB: capture and playback report `Unsupported` in slice 0 (S2 implements them).
//!
//! TODO(S2, fogar-realtime-audio): `cpal = "=0.18.2"` CoreAudio host; open the default input at
//! its DEFAULT rate/format (never request 16 kHz from hardware); call `play()` (no auto-start
//! since 0.18); the data callback only downmixes and `rtrb 0.4.1` `push_partial_slice`es (no
//! alloc/locks/logging/syscalls/Waker::wake); a dsp thread at QOS_CLASS_UTILITY resamples with
//! rubato `Fft` 48k->16k and feeds earshot VAD; poll `kAudioDevicePropertyDeviceUID` of the
//! default input every 2-5 s (cpal input does not follow the default device); rebuild on
//! `DeviceNotAvailable`/`StreamInvalidated` with backoff 250 ms -> 30 s.
//! Permission: `AVCaptureDevice.authorizationStatus(for: .audio)` (never prompts) for
//! `mic_permission`; the request happens only on user action from the UI. Needs
//! `NSMicrophoneUsageDescription` + `com.apple.security.device.audio-input` (hardened runtime).
//! NOTE: the S0 port delivers chunks via `EventSink::push` (tokio `try_send` wakes a Waker), which
//! is NOT allowed on the CoreAudio RT thread: the S2 adapter pushes from its dsp thread instead.

use fogar_ports::{
    audio::{
        ActiveCapture, ActivePlayback, AudioChunk, AudioError, AudioFormat, AudioSink, AudioSinkCaps, AudioSource,
        AudioSourceCaps, MicPermission,
    },
    events::EventStream,
};
use std::num::NonZeroUsize;

#[derive(Debug, Clone, Copy, Default)]
pub struct MacAudioSource;

impl AudioSource for MacAudioSource {
    fn capabilities(&self) -> AudioSourceCaps {
        AudioSourceCaps::NONE
    }
    fn mic_permission(&self) -> MicPermission {
        MicPermission::Unknown
    }
    fn open(&self, _capacity: NonZeroUsize) -> Result<(Box<dyn ActiveCapture>, EventStream<AudioChunk>), AudioError> {
        Err(AudioError::Unsupported("macOS capture arrives in S2"))
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MacAudioSink;

impl AudioSink for MacAudioSink {
    fn capabilities(&self) -> AudioSinkCaps {
        AudioSinkCaps::NONE
    }
    fn open(&self, _format: AudioFormat) -> Result<Box<dyn ActivePlayback>, AudioError> {
        Err(AudioError::Unsupported("macOS playback arrives in S2"))
    }
}
