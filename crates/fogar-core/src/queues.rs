//! Every queue in the agent, its capacity and its full-policy (fogar-longrun-reliability rule 2).
//! Changing a capacity or a policy is a reviewed change: update this table and the tests in
//! `tests/backpressure.rs`.
//!
//! | Queue | Producer -> consumer | Capacity | When full |
//! |---|---|---|---|
//! | mic chunks (`EventStream<AudioChunk>`) | audio adapter -> engine | `MIC_CHUNKS` (~1 s of 20 ms chunks) | adapter drops NEWEST, counts (`EventStream::dropped`); never blocks the audio thread |
//! | uplink audio (`out` channel, `Frame::Binary`) | engine -> link session | shares `OUTBOUND` | shed NEWEST, count `uplink_audio_shed` (`try_send`); the gateway would close a slow client anyway |
//! | uplink control (`out` channel, `Frame::Text`) | engine -> link session | shares `OUTBOUND` | must-deliver while connected: `reserve()` with `write_timeout`; on deadline count `uplink_control_dropped` and return the turn machine to Idle (the session itself is kept) |
//! | inbound JSON events | link session -> engine | `INBOUND` messages AND `INBOUND_BYTES` bytes | must-deliver: byte charge + `reserve()` with `inbound_deadline` (< gateway `CLIENT_SEND_TIMEOUT_S` = 10 s); on deadline the session ends as `EngineStalled` and reconnects |
//! | inbound TTS audio | link session -> engine | shares `INBOUND` and `INBOUND_BYTES` | shed NEWEST, count `inbound_audio_shed` (`try_send` / `try_charge`); the session never waits on audio |
//! | playback buffer | engine -> audio sink adapter | adapter-owned, bounded | `ActivePlayback::write` accepts what fits; engine counts the rest in `tts_samples_dropped` |
//! | link state / health | many -> readers | `watch` (1) | coalesce: latest value wins |
//! | IPC connections | IPC adapter -> service | `IPC_MAX_CONNS` concurrent (subscribers included) | further accepts wait (the accept branch is disabled while full); a connection idle for `Config::ipc_idle_timeout` (30 s) is closed, so silent clients cannot hold the slots; a SUBSCRIBED connection is exempt from the idle close but each push runs under `write_timeout` and a miss closes it |
//! | status subscription | link view -> each subscribed connection | `watch` (1) | coalesce: latest value wins, a push is skipped when the status did not change; never a backlog |
//! | pairing generation | IPC pair/unpair -> link | `watch` (1) | coalesce: the link only needs "the record changed", then re-reads it |
//! | pairing operations | IPC connections -> gateway `/pair` | `PAIRING` in flight | admission control: a second `Pair` gets `PairFailure::Busy` at once (`try_acquire`); `Unpair` waits for the running one, which is bounded by its HTTP deadlines |
//!
//! Stale uplink frames left in the `out` channel when a session dies are discarded (and counted)
//! before the next session starts: they belonged to a turn that no longer exists.

use std::{num::NonZeroUsize, sync::Arc};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

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
/// Pairing operations in flight (one: the gateway's code is one-use).
pub const PAIRING: usize = 1;
/// Bytes of inbound frames (JSON text length / binary length) queued link -> engine at once
/// (review sec#4): with 1 MiB frames, `INBOUND` messages alone would allow 128 MiB.
pub const INBOUND_BYTES: usize = 4 << 20;

/// Byte budget shared by the link (charges) and the engine (releases by dropping the [`Charge`]
/// carried inside each queued message).
#[derive(Debug, Clone)]
pub(crate) struct ByteBudget {
    sem: Arc<Semaphore>,
    total: u32,
}

/// Bytes held by one queued message; released on drop.
#[derive(Debug)]
pub(crate) struct Charge(#[allow(dead_code)] OwnedSemaphorePermit);

impl ByteBudget {
    pub(crate) fn new(bytes: usize) -> Self {
        let total = u32::try_from(bytes.min(Semaphore::MAX_PERMITS)).unwrap_or(u32::MAX);
        Self {
            sem: Arc::new(Semaphore::new(total as usize)),
            total,
        }
    }

    /// A frame larger than the whole budget is charged the whole budget (it then waits for an
    /// empty queue instead of never fitting). Zero-length frames still cost 1.
    fn cost(&self, bytes: usize) -> u32 {
        u32::try_from(bytes)
            .unwrap_or(u32::MAX)
            .clamp(1, self.total.max(1))
    }

    /// Non-blocking (audio: shed when over budget).
    pub(crate) fn try_charge(&self, bytes: usize) -> Option<Charge> {
        self.sem
            .clone()
            .try_acquire_many_owned(self.cost(bytes))
            .ok()
            .map(Charge)
    }

    /// Waits for room (must-deliver JSON). Cancel-safe; callers bound it with a deadline.
    pub(crate) async fn charge(&self, bytes: usize) -> Option<Charge> {
        self.sem
            .clone()
            .acquire_many_owned(self.cost(bytes))
            .await
            .ok()
            .map(Charge)
    }

    /// Bytes currently charged.
    #[cfg(test)]
    pub(crate) fn in_use(&self) -> usize {
        (self.total as usize).saturating_sub(self.sem.available_permits())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_budget_bounds_queued_bytes_and_releases_on_drop() {
        let b = ByteBudget::new(INBOUND_BYTES);
        let mib = 1 << 20;
        let held: Vec<_> = (0..4).map(|_| b.try_charge(mib).unwrap()).collect();
        assert_eq!(b.in_use(), INBOUND_BYTES);
        assert!(
            b.try_charge(1).is_none(),
            "4 x 1 MiB fills the 4 MiB budget"
        );
        drop(held);
        assert_eq!(b.in_use(), 0);
        // Oversize frames are clamped to the whole budget rather than never fitting.
        let big = b.try_charge(64 * mib).unwrap();
        assert_eq!(b.in_use(), INBOUND_BYTES);
        drop(big);
        assert!(b.try_charge(0).is_some());
    }

    #[tokio::test]
    async fn must_deliver_charge_waits_for_room() {
        let b = ByteBudget::new(1000);
        let first = b.try_charge(800).unwrap();
        let waiter = b.charge(500);
        tokio::pin!(waiter);
        assert!(futures_poll_once(waiter.as_mut()).is_none());
        drop(first);
        assert!(waiter.await.is_some());
    }

    fn futures_poll_once<F: std::future::Future + Unpin>(f: F) -> Option<F::Output> {
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let mut f = f;
        match std::pin::Pin::new(&mut f).poll(&mut cx) {
            std::task::Poll::Ready(v) => Some(v),
            std::task::Poll::Pending => None,
        }
    }
}
