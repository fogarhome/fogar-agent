//! Fuzz target bodies. Each one asserts "no panic" plus a cheap invariant; the libFuzzer binaries
//! in `fuzz_targets/` and the stable smoke test in `tests/` call the same functions.

use fogar_host::ipc_wire::{decode_request, decode_response, encode_request, frame, take_frame};
use fogar_ports::gateway::GatewayAddr;
use fogar_ports::ipc::{IpcError, IpcRequest, MAX_FRAME_BYTES};
use fogar_proto::{
    AudioFrameError, CloseAction, DecodeError, classify_close, encode_tts_frame,
    limits::WS_MAX_FRAME_BYTES, parse_tts_frame,
};

/// Gateway text frame -> `ServerMsg`. The transport only hands over valid UTF-8.
pub fn decode_server(data: &[u8]) {
    let Ok(text) = core::str::from_utf8(data) else {
        return;
    };
    let r = fogar_proto::decode_server(text);
    if text.len() > WS_MAX_FRAME_BYTES {
        assert!(matches!(r, Err(DecodeError::TooLarge { .. })));
    }
}

/// Gateway binary frame -> TTS frame. Round-trips through the encoder when the PCM is even.
pub fn tts_frame(data: &[u8]) {
    match parse_tts_frame(data) {
        Ok(f) => {
            assert_eq!(f.pcm_s16le.len(), data.len() - 3);
            assert_eq!(
                f.sample_count() * 2 + usize::from(f.has_odd_tail()),
                f.pcm_s16le.len()
            );
            if !f.has_odd_tail() {
                let pcm: Vec<i16> = f.samples().collect();
                let mut out = Vec::new();
                encode_tts_frame(f.turn_seq, &pcm, &mut out);
                assert_eq!(out, data);
            }
        }
        Err(AudioFrameError::TooShort) => assert!(data.len() < 3),
        Err(AudioFrameError::BadMagic(m)) => assert_eq!(Some(&m), data.first()),
    }
}

/// Close code (first two bytes, big-endian) + reason text (the rest). The reason may only
/// matter for the legacy 1008 code: for every other code the code alone is the contract.
pub fn close_code(data: &[u8]) {
    let [hi, lo, rest @ ..] = data else {
        return;
    };
    let code = u16::from_be_bytes([*hi, *lo]);
    let reason = String::from_utf8_lossy(rest);
    let action = classify_close(code, &reason);
    if code != 1008 {
        assert_eq!(action, classify_close(code, ""));
    }
    if (4000..=4999).contains(&code) {
        assert_ne!(action, CloseAction::Unknown);
    }
}

/// A byte stream from a local UI peer: split into frames, decode each body as a request (the
/// daemon side) and as a response (the client side). A decoded request survives a
/// re-encode/decode unchanged, and a `Pair` address the core accepts (`GatewayAddr::parse`)
/// round-trips through its `Display` form.
pub fn ipc_frames(data: &[u8]) {
    let mut buf = data.to_vec();
    loop {
        let before = buf.clone();
        match take_frame(&mut buf) {
            Ok(None) => {
                assert_eq!(buf, before);
                break;
            }
            Err(IpcError::TooLarge { len, max }) => {
                assert!(len > max && max == MAX_FRAME_BYTES);
                break;
            }
            Err(e) => panic!("take_frame: unexpected error {e:?}"),
            Ok(Some(body)) => {
                let used = 4 + body.len();
                assert!(body.len() <= MAX_FRAME_BYTES);
                assert_eq!(buf[..], before[used..]);
                assert_eq!(
                    frame(&body).ok().as_deref().map(|v| &v[..]),
                    Some(&before[..used])
                );
                if let Ok(req) = decode_request(&body) {
                    if let IpcRequest::Pair { address, .. } = &req
                        && let Ok(addr) = GatewayAddr::parse(address)
                    {
                        assert_eq!(GatewayAddr::parse(&addr.to_string()).ok(), Some(addr));
                    }
                    let again = encode_request(&req)
                        .ok()
                        .and_then(|b| decode_request(&b).ok());
                    assert_eq!(again, Some(req));
                }
                let _ = decode_response(&body);
            }
        }
    }
}
