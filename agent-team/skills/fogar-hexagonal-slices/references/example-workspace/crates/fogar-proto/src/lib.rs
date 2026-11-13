//! fogar-proto: wire codec, `no_std` + `alloc`.
//!
//! PLACEHOLDER FRAMING. The real gateway protocol must be implemented from its spec;
//! this file only shows the *shape*: pure functions over byte slices, sans-I/O,
//! no allocation on decode of borrowed data, explicit versioning, non_exhaustive errors.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

use alloc::vec::Vec;

/// Protocol version this codec speaks. Bump on breaking wire changes.
pub const PROTO_VERSION: u8 = 1;

/// A decoded message. `#[non_exhaustive]`: adding variants is not a breaking change
/// for downstream crates (they must keep a `_` arm).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Message<'a> {
    Heartbeat {
        seq: u32,
    },
    /// Agent work mode changed (0 = paused, 1 = reduced, 2 = full).
    Status {
        seq: u32,
        mode: u8,
    },
    /// Borrowed payload: decoding does not allocate.
    Opaque {
        kind: u8,
        body: &'a [u8],
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DecodeError {
    #[error("frame truncated: need {need} bytes, have {have}")]
    Truncated { need: usize, have: usize },
    #[error("unsupported protocol version {0}")]
    Version(u8),
    #[error("unknown message kind {0}")]
    UnknownKind(u8),
}

const K_HEARTBEAT: u8 = 1;
const K_STATUS: u8 = 2;
const K_OPAQUE_MIN: u8 = 0x80;

/// Encode into a caller-provided buffer (lets the caller reuse allocations in a long-running daemon).
pub fn encode_into(msg: &Message<'_>, out: &mut Vec<u8>) {
    out.push(PROTO_VERSION);
    match msg {
        Message::Heartbeat { seq } => {
            out.push(K_HEARTBEAT);
            out.extend_from_slice(&seq.to_le_bytes());
        }
        Message::Status { seq, mode } => {
            out.push(K_STATUS);
            out.extend_from_slice(&seq.to_le_bytes());
            out.push(*mode);
        }
        Message::Opaque { kind, body } => {
            out.push(*kind);
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(body);
        }
    }
}

pub fn decode(buf: &[u8]) -> Result<Message<'_>, DecodeError> {
    let need = |n: usize| {
        if buf.len() < n {
            Err(DecodeError::Truncated {
                need: n,
                have: buf.len(),
            })
        } else {
            Ok(())
        }
    };
    need(2)?;
    if buf[0] != PROTO_VERSION {
        return Err(DecodeError::Version(buf[0]));
    }
    let u32_at = |i: usize| u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
    match buf[1] {
        K_HEARTBEAT => {
            need(6)?;
            Ok(Message::Heartbeat { seq: u32_at(2) })
        }
        K_STATUS => {
            need(7)?;
            Ok(Message::Status {
                seq: u32_at(2),
                mode: buf[6],
            })
        }
        k if k >= K_OPAQUE_MIN => {
            need(6)?;
            let len = u32_at(2) as usize;
            need(6 + len)?;
            Ok(Message::Opaque {
                kind: k,
                body: &buf[6..6 + len],
            })
        }
        k => Err(DecodeError::UnknownKind(k)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let msgs = [
            Message::Heartbeat { seq: 7 },
            Message::Status { seq: 8, mode: 2 },
            Message::Opaque {
                kind: 0x90,
                body: b"hi",
            },
        ];
        for m in msgs {
            let mut v = Vec::new();
            encode_into(&m, &mut v);
            assert_eq!(decode(&v).unwrap(), m);
        }
    }

    #[test]
    fn truncated_is_error_not_panic() {
        for n in 0..6 {
            let mut v = Vec::new();
            encode_into(&Message::Heartbeat { seq: 1 }, &mut v);
            v.truncate(n);
            assert!(decode(&v).is_err());
        }
    }
}
