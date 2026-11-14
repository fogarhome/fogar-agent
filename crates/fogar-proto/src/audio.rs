//! Binary audio frames (spec §12). All functions are allocation-free except the encoders,
//! which append into a caller-owned (reusable) buffer.

use alloc::vec::Vec;
use core::fmt;

/// CERTAIN: upload is PCM f32le, 16 kHz, mono, no header, any frame size <= 1 MiB.
pub const UPLOAD_RATE_HZ: u32 = 16_000;
/// CERTAIN: download is a 3-byte header followed by PCM16 LE, 24 kHz, mono.
pub const TTS_MAGIC: u8 = 0xA1;
pub const TTS_HEADER_LEN: usize = 3;
pub const TTS_RATE_HZ: u32 = 24_000;
pub const TTS_BYTES_PER_SEC: u32 = 48_000;
/// LIKELY (7e): frames <= 200 ms, but the docs say not to rely on size.
pub const TTS_MAX_FRAME_HINT_BYTES: usize = 9_600;
/// CERTAIN (7e): the server stays at most this far ahead of playback.
pub const AUDIO_LEAD_MS: u32 = 2_000;

/// A parsed TTS frame borrowing the input buffer (zero-copy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TtsFrame<'a> {
    pub turn_seq: u16,
    /// Raw PCM16 little-endian bytes. Length should be even (not guaranteed by the docs).
    pub pcm_s16le: &'a [u8],
}

impl<'a> TtsFrame<'a> {
    /// Iterates the samples without allocating. A trailing odd byte is ignored
    /// (see [`TtsFrame::has_odd_tail`]).
    pub fn samples(&self) -> impl ExactSizeIterator<Item = i16> + 'a {
        self.pcm_s16le
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
    }
    pub fn sample_count(&self) -> usize {
        self.pcm_s16le.len() / 2
    }
    pub fn has_odd_tail(&self) -> bool {
        self.pcm_s16le.len() % 2 == 1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFrameError {
    TooShort,
    BadMagic(u8),
}

impl fmt::Display for AudioFrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioFrameError::TooShort => f.write_str("audio frame shorter than its 3-byte header"),
            AudioFrameError::BadMagic(m) => write!(f, "audio frame has unknown magic 0x{m:02x}"),
        }
    }
}

impl core::error::Error for AudioFrameError {}

/// Zero-copy parse. Callers discard the frame if `turn_seq` != the latest `TurnStart.turn_seq`.
pub fn parse_tts_frame(buf: &[u8]) -> Result<TtsFrame<'_>, AudioFrameError> {
    let [magic, hi, lo, pcm @ ..] = buf else {
        return Err(AudioFrameError::TooShort);
    };
    if *magic != TTS_MAGIC {
        return Err(AudioFrameError::BadMagic(*magic));
    }
    Ok(TtsFrame {
        turn_seq: u16::from_be_bytes([*hi, *lo]),
        pcm_s16le: pcm,
    })
}

/// Appends a TTS frame (used by the fake gateway and tests; the agent never sends these).
pub fn encode_tts_frame(turn_seq: u16, pcm: &[i16], out: &mut Vec<u8>) {
    out.reserve(TTS_HEADER_LEN + pcm.len() * 2);
    out.push(TTS_MAGIC);
    out.extend_from_slice(&turn_seq.to_be_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
}

/// Appends mono f32 samples as f32le (the only upload format the gateway accepts).
pub fn encode_upload_f32le(samples: &[f32], out: &mut Vec<u8>) {
    out.reserve(samples.len() * 4);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
}

/// Decodes an upload frame without allocating (fake gateway, tests). Trailing bytes that do not
/// form a whole sample are ignored (spec UNKNOWN #14: the gateway does not validate this).
pub fn decode_upload_f32le(buf: &[u8]) -> impl ExactSizeIterator<Item = f32> + '_ {
    buf.chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn tts_vector() {
        // Spec §7 example: 0xA1 0x00 0x01 PCM16...
        let buf = [0xA1, 0x00, 0x01, 0x34, 0x12, 0xff, 0xff];
        let f = parse_tts_frame(&buf).unwrap();
        assert_eq!(f.turn_seq, 1);
        assert_eq!(f.samples().collect::<Vec<_>>(), vec![0x1234, -1]);
        assert!(!f.has_odd_tail());
    }

    #[test]
    fn tts_errors() {
        assert_eq!(parse_tts_frame(&[]), Err(AudioFrameError::TooShort));
        assert_eq!(parse_tts_frame(&[0xA1, 0]), Err(AudioFrameError::TooShort));
        assert_eq!(
            parse_tts_frame(&[0xA2, 0, 0]),
            Err(AudioFrameError::BadMagic(0xA2))
        );
        // Header only is a valid (empty) frame.
        assert_eq!(
            parse_tts_frame(&[0xA1, 0xff, 0xff]).unwrap().turn_seq,
            u16::MAX
        );
    }

    #[test]
    fn odd_tail_is_reported_not_fatal() {
        let f = parse_tts_frame(&[0xA1, 0, 2, 1, 0, 9]).unwrap();
        assert!(f.has_odd_tail());
        assert_eq!(f.sample_count(), 1);
    }

    #[test]
    fn upload_vector() {
        let mut out = Vec::new();
        encode_upload_f32le(&[1.0, -0.5], &mut out);
        assert_eq!(out, [0, 0, 0x80, 0x3f, 0, 0, 0, 0xbf]);
        assert_eq!(
            decode_upload_f32le(&out).collect::<Vec<_>>(),
            vec![1.0, -0.5]
        );
    }
}
