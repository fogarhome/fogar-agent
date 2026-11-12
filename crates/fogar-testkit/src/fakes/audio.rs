use fogar_ports::{
    Support,
    audio::{
        ActiveCapture, ActivePlayback, AudioChunk, AudioError, AudioFormat, AudioSink, AudioSinkCaps,
        AudioSource, AudioSourceCaps, MicPermission,
    },
    events::{EventSink, EventStream, Pushed, event_channel},
};
use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

/// 20 ms at 16 kHz.
pub const CHUNK_SAMPLES: usize = 320;

/// `n` chunks of a 440 Hz tone at amplitude `amp` (a scripted "utterance").
pub fn tone_chunks(n: usize, amp: f32) -> Vec<AudioChunk> {
    let mut t = 0usize;
    (0..n)
        .map(|_| {
            let samples = (0..CHUNK_SAMPLES)
                .map(|_| {
                    t += 1;
                    amp * (t as f32 * 2.0 * std::f32::consts::PI * 440.0 / 16_000.0).sin()
                })
                .collect();
            AudioChunk { samples }
        })
        .collect()
}

pub fn silence_chunks(n: usize) -> Vec<AudioChunk> {
    (0..n).map(|_| AudioChunk { samples: vec![0.0; CHUNK_SAMPLES] }).collect()
}

#[derive(Debug, Default)]
struct MicState {
    sink: Option<EventSink<AudioChunk>>,
    opens: u32,
}

/// Fake microphone. The test pushes chunks as the OS callback would (never blocking; full =>
/// dropped and counted, exactly the port contract).
#[derive(Debug, Clone)]
pub struct FakeMic {
    caps: AudioSourceCaps,
    format: AudioFormat,
    permission: MicPermission,
    state: Arc<Mutex<MicState>>,
}

impl FakeMic {
    /// 16 kHz mono, capture supported, permission granted.
    pub fn healthy() -> Self {
        let mut caps = AudioSourceCaps::NONE;
        caps.capture = Support::Native;
        caps.permission_query = Support::Native;
        Self { caps, format: AudioFormat::mono(16_000), permission: MicPermission::Granted, state: Arc::default() }
    }
    /// No capture at all (e.g. a headless Linux box).
    pub fn minimal() -> Self {
        Self { caps: AudioSourceCaps::NONE, format: AudioFormat::mono(16_000), permission: MicPermission::Unknown, state: Arc::default() }
    }
    pub fn denied() -> Self {
        Self { permission: MicPermission::Denied, ..Self::healthy() }
    }
    pub fn with_format(mut self, f: AudioFormat) -> Self {
        self.format = f;
        self
    }
    /// Pushes one chunk; `None` if no capture is open.
    pub fn push(&self, c: AudioChunk) -> Option<Pushed> {
        self.state.lock().unwrap().sink.as_ref().map(|s| s.push(c))
    }
    pub fn push_all(&self, cs: Vec<AudioChunk>) -> Vec<Option<Pushed>> {
        cs.into_iter().map(|c| self.push(c)).collect()
    }
    pub fn is_open(&self) -> bool {
        self.state.lock().unwrap().sink.as_ref().is_some_and(|s| !s.is_closed())
    }
    pub fn opens(&self) -> u32 {
        self.state.lock().unwrap().opens
    }
    pub fn dropped(&self) -> u64 {
        self.state.lock().unwrap().sink.as_ref().map_or(0, |s| s.dropped())
    }
}

#[derive(Debug)]
struct FakeCapture {
    format: AudioFormat,
    state: Arc<Mutex<MicState>>,
}

impl ActiveCapture for FakeCapture {
    fn format(&self) -> AudioFormat {
        self.format
    }
}

impl Drop for FakeCapture {
    fn drop(&mut self) {
        if let Ok(mut st) = self.state.lock() {
            st.sink = None;
        }
    }
}

impl AudioSource for FakeMic {
    fn capabilities(&self) -> AudioSourceCaps {
        self.caps
    }
    fn mic_permission(&self) -> MicPermission {
        self.permission
    }
    fn open(&self, capacity: NonZeroUsize) -> Result<(Box<dyn ActiveCapture>, EventStream<AudioChunk>), AudioError> {
        if !self.caps.capture.is_available() {
            return Err(AudioError::Unsupported("capture"));
        }
        if self.permission == MicPermission::Denied {
            return Err(AudioError::PermissionDenied);
        }
        let (tx, rx) = event_channel(capacity);
        let mut st = self.state.lock().unwrap();
        st.sink = Some(tx);
        st.opens += 1;
        Ok((Box::new(FakeCapture { format: self.format, state: self.state.clone() }), rx))
    }
}

#[derive(Debug, Default)]
struct SpeakerState {
    /// Every accepted sample (when recording is on).
    pub played: Vec<i16>,
    pub accepted: u64,
    pub refused: u64,
    pub queued: usize,
    pub stops: u32,
    pub opens: u32,
    pub open_now: u32,
}

/// Fake speaker with a bounded buffer of `capacity` samples. Nothing "plays" by itself: the test
/// drains the buffer with [`FakeSpeaker::drain`] (no timers in the fake).
#[derive(Debug, Clone)]
pub struct FakeSpeaker {
    caps: AudioSinkCaps,
    capacity: usize,
    record: bool,
    state: Arc<Mutex<SpeakerState>>,
}

impl FakeSpeaker {
    /// Plenty of room (10 s at 24 kHz), records samples.
    pub fn healthy() -> Self {
        let mut caps = AudioSinkCaps::NONE;
        caps.playback = Support::Native;
        Self { caps, capacity: 240_000, record: true, state: Arc::default() }
    }
    pub fn minimal() -> Self {
        Self { caps: AudioSinkCaps::NONE, capacity: 0, record: false, state: Arc::default() }
    }
    pub fn with_capacity(mut self, samples: usize) -> Self {
        self.capacity = samples;
        self
    }
    /// Do not keep samples (long soaks).
    pub fn without_recording(mut self) -> Self {
        self.record = false;
        self
    }
    pub fn drain(&self) {
        self.state.lock().unwrap().queued = 0;
    }
    pub fn snapshot(&self) -> SpeakerSnapshot {
        let st = self.state.lock().unwrap();
        SpeakerSnapshot {
            played: st.played.len(),
            accepted: st.accepted,
            refused: st.refused,
            queued: st.queued,
            stops: st.stops,
            opens: st.opens,
            open_now: st.open_now,
        }
    }
    pub fn played_samples(&self) -> Vec<i16> {
        self.state.lock().unwrap().played.clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeakerSnapshot {
    pub played: usize,
    pub accepted: u64,
    pub refused: u64,
    pub queued: usize,
    pub stops: u32,
    pub opens: u32,
    pub open_now: u32,
}

#[derive(Debug)]
struct FakePlayback {
    format: AudioFormat,
    capacity: usize,
    record: bool,
    state: Arc<Mutex<SpeakerState>>,
}

impl ActivePlayback for FakePlayback {
    fn format(&self) -> AudioFormat {
        self.format
    }
    fn write(&mut self, pcm: &[i16]) -> usize {
        let mut st = self.state.lock().unwrap();
        let room = self.capacity.saturating_sub(st.queued);
        let n = pcm.len().min(room);
        st.queued += n;
        st.accepted += n as u64;
        st.refused += (pcm.len() - n) as u64;
        if self.record {
            st.played.extend_from_slice(&pcm[..n]);
        }
        n
    }
    fn stop(&mut self) {
        let mut st = self.state.lock().unwrap();
        st.queued = 0;
        st.stops += 1;
    }
    fn queued_samples(&self) -> usize {
        self.state.lock().unwrap().queued
    }
}

impl Drop for FakePlayback {
    fn drop(&mut self) {
        if let Ok(mut st) = self.state.lock() {
            st.open_now = st.open_now.saturating_sub(1);
            st.queued = 0;
        }
    }
}

impl AudioSink for FakeSpeaker {
    fn capabilities(&self) -> AudioSinkCaps {
        self.caps
    }
    fn open(&self, format: AudioFormat) -> Result<Box<dyn ActivePlayback>, AudioError> {
        if !self.caps.playback.is_available() {
            return Err(AudioError::Unsupported("playback"));
        }
        let mut st = self.state.lock().unwrap();
        st.opens += 1;
        st.open_now += 1;
        Ok(Box::new(FakePlayback { format, capacity: self.capacity, record: self.record, state: self.state.clone() }))
    }
}
