//! Every queue in the agent, its capacity and its full-policy (fogar-longrun-reliability rule 2).
//! Changing a capacity or a policy is a reviewed change: update this table and the tests in
//! `tests/backpressure.rs`.
//!
//! | Queue | Producer -> consumer | Capacity | When full |
//! |---|---|---|---|
//! | mic chunks (`EventStream<AudioChunk>`) | audio adapter -> engine | `MIC_CHUNKS` (~1 s of 20 ms chunks) | adapter drops NEWEST, counts (`EventStream::dropped`); never blocks the audio thread |
//! | uplink audio (`out` channel, `Frame::Binary`) | engine -> link session | shares `OUTBOUND` | shed NEWEST, count `uplink_audio_shed` (`try_send`); the gateway would close a slow client anyway |
//! | uplink control (`out` channel, `Frame::Text`) | engine -> link session | shares `OUTBOUND` | must-deliver while connected: `reserve()` with `write_timeout`; on deadline count `uplink_control_dropped` and reset the turn (the session is then considered stalled) |
//! | inbound JSON events | link session -> engine | `INBOUND` | must-deliver: `reserve()` with `inbound_deadline` (< gateway `CLIENT_SEND_TIMEOUT_S` = 10 s); on deadline the session ends as `EngineStalled` and reconnects |
//! | inbound TTS audio | link session -> engine | shares `INBOUND` | shed NEWEST, count `inbound_audio_shed` (`try_send`); the session never waits on audio |
//! | playback buffer | engine -> audio sink adapter | adapter-owned, bounded | `ActivePlayback::write` accepts what fits; engine counts the rest in `tts_samples_dropped` |
//! | link state / health | many -> readers | `watch` (1) | coalesce: latest value wins |
//! | IPC connections | IPC adapter -> service | `IPC_MAX_CONNS` concurrent | further accepts wait (the accept branch is disabled while full) |
//!
//! Stale uplink frames left in the `out` channel when a session dies are discarded (and counted)
//! before the next session starts: they belonged to a turn that no longer exists.

use std::num::NonZeroUsize;

const fn nz(n: usize) -> NonZeroUsize {
    match NonZeroUsize::new(n) {
        Some(v) => v,
        None => NonZeroUsize::MIN,
    }
}

/// 50 x 20 ms = 1 s of capture.
pub const MIC_CHUNKS: NonZeroUsize = nz(50);
/// Engine -> link session.
pub const OUTBOUND: usize = 64;
/// Link session -> engine.
pub const INBOUND: usize = 128;
/// Concurrent local UI connections.
pub const IPC_MAX_CONNS: usize = 4;
