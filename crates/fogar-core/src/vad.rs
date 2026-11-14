//! Minimal energy VAD for slice 0 (the "fake VAD" of the walking skeleton): RMS threshold with a
//! hangover. Pure and allocation-free. S2 replaces it with earshot (fogar-realtime-audio) behind the
//! same `VadEvent` interface; no port is involved because VAD is pure logic.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VadConfig {
    /// RMS in [0, 1] above which a chunk counts as voiced.
    pub threshold_rms: f32,
    /// Consecutive unvoiced chunks that end an utterance.
    pub hangover_chunks: u32,
    /// Longest utterance in chunks; longer speech is cut (bounded upload per turn).
    pub max_utterance_chunks: u32,
}

impl Default for VadConfig {
    fn default() -> Self {
        // 20 ms chunks: 300 ms hangover, 30 s max utterance.
        Self {
            threshold_rms: 0.02,
            hangover_chunks: 15,
            max_utterance_chunks: 1500,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadEvent {
    Silence,
    /// First voiced chunk: this chunk belongs to the utterance.
    SpeechStart,
    /// Inside an utterance (voiced or within the hangover): this chunk belongs to it.
    Speech,
    /// Utterance over: this chunk does NOT belong to it.
    SpeechEnd,
}

#[derive(Debug, Clone)]
pub struct EnergyVad {
    cfg: VadConfig,
    in_speech: bool,
    quiet_run: u32,
    length: u32,
}

impl EnergyVad {
    pub fn new(cfg: VadConfig) -> Self {
        Self {
            cfg,
            in_speech: false,
            quiet_run: 0,
            length: 0,
        }
    }

    pub fn reset(&mut self) {
        self.in_speech = false;
        self.quiet_run = 0;
        self.length = 0;
    }

    pub fn in_speech(&self) -> bool {
        self.in_speech
    }

    pub fn process(&mut self, samples: &[f32]) -> VadEvent {
        let voiced = rms(samples) >= self.cfg.threshold_rms;
        if !self.in_speech {
            if voiced {
                self.in_speech = true;
                self.quiet_run = 0;
                self.length = 1;
                return VadEvent::SpeechStart;
            }
            return VadEvent::Silence;
        }
        self.length = self.length.saturating_add(1);
        self.quiet_run = if voiced {
            0
        } else {
            self.quiet_run.saturating_add(1)
        };
        if self.quiet_run >= self.cfg.hangover_chunks || self.length > self.cfg.max_utterance_chunks
        {
            self.reset();
            return VadEvent::SpeechEnd;
        }
        VadEvent::Speech
    }
}

/// Root mean square; 0 for an empty or non-finite chunk.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    let v = (sum / samples.len() as f32).sqrt();
    if v.is_finite() { v } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utterance_with_hangover() {
        let mut v = EnergyVad::new(VadConfig {
            threshold_rms: 0.1,
            hangover_chunks: 2,
            max_utterance_chunks: 100,
        });
        let loud = [0.5f32; 320];
        let quiet = [0.0f32; 320];
        assert_eq!(v.process(&quiet), VadEvent::Silence);
        assert_eq!(v.process(&loud), VadEvent::SpeechStart);
        assert_eq!(v.process(&loud), VadEvent::Speech);
        assert_eq!(v.process(&quiet), VadEvent::Speech);
        assert_eq!(v.process(&quiet), VadEvent::SpeechEnd);
        assert_eq!(v.process(&quiet), VadEvent::Silence);
    }

    #[test]
    fn long_speech_is_cut_and_nan_is_silence() {
        let mut v = EnergyVad::new(VadConfig {
            threshold_rms: 0.1,
            hangover_chunks: 2,
            max_utterance_chunks: 3,
        });
        let loud = [0.5f32; 4];
        assert_eq!(v.process(&loud), VadEvent::SpeechStart);
        assert_eq!(v.process(&loud), VadEvent::Speech);
        assert_eq!(v.process(&loud), VadEvent::Speech);
        assert_eq!(v.process(&loud), VadEvent::SpeechEnd);
        assert_eq!(rms(&[f32::NAN]), 0.0);
        assert_eq!(rms(&[]), 0.0);
    }
}
