//! Every queue in the agent, its capacity and its full-policy (fogar-longrun-reliability rule 2).
//! Changing a capacity or a policy is a reviewed change: update this table and the tests in
//! `tests/backpressure.rs`.
//!
//! | Queue | Producer -> consumer | Capacity | When full |
//! |---|---|---|---|
//! | mic chunks (`EventStream<AudioChunk>`) | audio adapter -> capture gate | `MIC_CHUNKS` (~1 s of 20 ms chunks) | adapter drops NEWEST, counts (`EventStream::dropped`); never blocks the audio thread. Only open while a take is open (`capture.rs`, the consent gate) |
//! | capture device jobs (blocking `AudioSource::open` / drop of `ActiveCapture`, issue #24) | capture gate -> tokio blocking pool, one owned `JoinSet` | `CAPTURE_JOBS` opens in flight, plus the releases | admission control: a `CaptureStart` while an open runs (also a late one past its deadline) is refused `Busy` at once; a release is never refused (one per take); a start arriving while the previous take's release runs waits for it (still under `capture::OPEN_DEADLINE` from the request) so the device is never opened while closing; each job is bounded by the adapter (`OPEN_BLOCK_MAX` / `DROP_BLOCK_MAX`) and an open also by `capture::OPEN_DEADLINE` on the core side (late result released); on stop the gate waits `capture::RELEASE_WAIT_ON_STOP`, then detaches |
//! | permission request waiters | IPC `RequestMicPermission` -> capture gate | `IPC_MAX_CONNS` on one pending prompt | an extra waiter is answered at once with the current status; each waiter's IPC side gives up after `Config::mic_permission_wait` and gets the current status |
//! | uplink (S4 PR 4a) | — | none | there is no uplink queue: the chat link is `GET /events` (read only) and the chat writes are REST calls (row below). PR 5's voice link brings one back |
//! | `/events` reader (`SseReader`) | stream body -> link | `fogar_proto::sse::MAX_EVENT_BYTES` (128 KiB, GUESSED) per event, one chunk (`MAX_CHUNK_BYTES`) of parsed items at a time | an oversize event is skipped without buffering, counted (`events_oversize`) and turned into a must-deliver `Inbound::Resync` (re-fetch) |
//! | inbound events (`Inbound`) | link -> engine | `INBOUND` messages AND `INBOUND_BYTES` bytes (each `Event` charged by its data length) | must-deliver: byte charge + `reserve()`, each under `inbound_deadline`; the resume id (`Last-Event-ID`) advances only after the engine accepted the event. On deadline the stream is dropped and resumed after a backoff from the last accepted id (the gateway replays, nothing is lost) |
//! | push `audio` events | link | none | dropped in the link and counted (`events_audio_dropped`): nothing plays in 4a |
//! | unseen TTS buffer (one turn's audio waiting to be confirmed visible, or for the output to open; idle in 4a: nothing feeds the gate and IPC v6 has no `TurnVisible`, PR 5 decides how a turn is confirmed) | engine -> playback gate (`playback.rs`) | `playback::UNSEEN_MAX_SAMPLES` (3 s at 24 kHz, ~144 KB) | unconfirmed: the turn is MUTED, its audio discarded and counted (`tts_turns_muted`, `tts_samples_muted`); also when `playback::VISIBLE_DEADLINE` (2 s from `turn_start`) passes. Confirmed but the device is not ready: drop NEWEST, count `tts_samples_dropped`; a failed or late open drops what waited (counted) |
//! | playback device jobs (blocking `AudioSink::open` / drop of `ActivePlayback`, issues #24/#25) | playback gate -> tokio blocking pool, one owned `JoinSet` (through `capture::spawn_device_job`, issue #31) | one open in flight, plus the releases | no second open while one runs (also a late one past `playback::OPEN_DEADLINE`, whose result is released) or while a release runs; opens only for confirmed audio and after the backoff (500 ms doubling to 8 s) of a failed open or a dead output; a release is never refused (one per opened output); on stop the gate waits 500 ms, then detaches |
//! | playback buffer | playback gate -> audio sink adapter | adapter-owned, bounded | `ActivePlayback::write` accepts what fits; the gate counts the rest in `tts_samples_dropped`; a dead output (`is_alive` false) gets nothing and is released |
//! | engine commands (typed text, cancel, capture start/end, permission request, turn visible) | IPC connections -> engine | `ENGINE_COMMANDS` | the IPC answers "busy" at once (`try_send`, never waits) and counts `engine_commands_busy`; each command carries a `oneshot` the IPC awaits under `write_timeout`, and a command whose reply was abandoned is skipped, not executed late |
//! | conversation view | engine -> each conversation-subscribed connection | `watch` (1) | coalesce: latest view wins, never a backlog; each push runs under `write_timeout` and a miss closes the connection; the view itself is bounded (`CONVERSATION_MAX_TURNS`, `CONVERSATION_MAX_TEXT_BYTES`) |
//! | link state / health | many -> readers | `watch` (1) | coalesce: latest value wins |
//! | IPC connections | IPC adapter -> service | `IPC_MAX_CONNS` concurrent (subscribers included) | further accepts wait (the accept branch is disabled while full); a connection idle for `Config::ipc_idle_timeout` (30 s) is closed, so silent clients cannot hold the slots; a SUBSCRIBED connection is exempt from the idle close but each push runs under `write_timeout` and a miss closes it |
//! | status subscription | link view -> each subscribed connection | `watch` (1) | coalesce: latest value wins, a push is skipped when the status did not change; never a backlog |
//! | engine REST calls (S4 PR 4a) | engine -> gateway over `GatewayHttp`, one owned `JoinSet` drained in the engine's `select!` | writes (send, create, cancel): `REST_IN_FLIGHT`; reads: at most one list, one `/auth/me` and one open step (state or history) at a time (a newer open aborts the older step) | admission control: a write beyond `REST_IN_FLIGHT` is refused at once (the IPC answers busy, counted `rest_busy`); a list refresh asked while one runs is folded into one more after it; every call runs under `Config::http_timeout` and its body is capped by the port (`HttpCall::max_body`); signing out aborts the set |
//! | open-conversation buffer | link events of the conversation being opened (or of conversations not listed when a create started, while it is in flight) -> store | `conversation::BUFFER_EVENTS` (256) and `BUFFER_BYTES` (64 KiB, each event charged its SSE data length) | overflow: everything buffered is dropped (memory freed), counted `open_overflows`, and the conversation is opened again (state + history) |
//! | conversation list | engine -> each list-subscribed connection (`ListConversations`) | `watch` (1) | coalesce: latest list wins, published only when it changed (compared before it is built), never a backlog; ≤ `conversation::LIST_MAX` items; each push under `write_timeout`, a miss closes the connection |
//! | conversation store (`Chat`) | engine -> tests | `watch` (1) | coalesce: latest value wins; bounded inside (`LIST_MAX` items, `MAX_ENTRIES` and `CONVERSATION_MAX_TEXT_BYTES` for the open one) |
//! | account generation | IPC login/logout -> link | `watch` (1) | coalesce: the link only needs "the account changed", then re-reads it |
//! | reconnect request | IPC reconnect -> parked link | `watch` (1) | coalesce: many clicks are one new attempt; a request seen before the link parked is discarded |
//! | discovery probes | IPC `Discover` -> gateway `/.well-known` | one `JoinSet` per request, at most `MAX_TAILNET_PEERS + 2` probes (saved, local, tailnet peers) | no queue: every probe runs at once under `probe_timeout`, after the tailnet query under `tailnet_timeout`; a cancelled request drops the set and aborts them |
//! | account operations | IPC connections -> gateway `/auth/login`, `/auth/logout` | `ACCOUNT` in flight | admission control: a second `Login` gets `LoginFailure::Busy` at once (`try_acquire`); `Logout` waits for the running one, which is bounded by its deadlines (tailnet check, probe, `account_timeout`) |

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
/// Blocking capture opens in flight at once (`capture.rs`); releases are never refused.
pub const CAPTURE_JOBS: usize = 1;
/// Link -> engine.
pub const INBOUND: usize = 128;
/// IPC -> engine commands. With `IPC_MAX_CONNS` connections answering one request at a time it
/// fills only while the engine is stalled (abandoned commands wait to be skipped).
pub const ENGINE_COMMANDS: usize = 8;
/// Concurrent local UI connections.
pub const IPC_MAX_CONNS: usize = 4;
/// Conversation writes (send, create, cancel) in flight at once; more are refused busy.
pub const REST_IN_FLIGHT: usize = 4;
/// Account operations (sign-in / sign-out) in flight: one, so two sign-ins cannot race to store.
pub const ACCOUNT: usize = 1;
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
    // shortcut: no inbound audio is queued in S4 PR 4a; PR 5's voice link sheds with it again.
    #[cfg_attr(not(test), allow(dead_code))]
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
