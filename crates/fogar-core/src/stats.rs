//! Health and leak/growth probes (fogar-longrun-reliability "Leak and growth probes"). Counters are
//! relaxed atomics (statistics only, nothing is published through them); link state is a `watch`
//! (coalescing: latest value wins).

use fogar_proto::CloseAction;
use std::sync::atomic::{AtomicU64, Ordering};

/// State of the gateway connection as seen by the supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    Starting,
    Connecting {
        attempt: u32,
    },
    /// Session established (`ready` received).
    Ready {
        session: u64,
    },
    /// Waiting before the next attempt.
    Backoff {
        attempt: u32,
        last_close: Option<CloseAction>,
    },
    /// No account: the agent is signed out (the name predates accounts; on the wire
    /// `not_paired`). Waits (no timer) for an account change.
    NotPaired,
    /// Parked: retrying would loop (token invalid, agent refused, client bug, missing
    /// scope). Stays alive and reports health; leaves this state on restart, a new sign-in, or an
    /// IPC `Reconnect` when [`LinkState::user_may_reconnect`].
    Parked {
        reason: CloseAction,
    },
    ShuttingDown,
    Stopped,
}

impl LinkState {
    /// Parked for a reason only the person can settle, so their `Reconnect` gets one new attempt:
    /// 4011 (the admin may have granted the scope meanwhile) 4020/4021 (the admin may have
    /// allowed the agent meanwhile) and 4003 (the person may have updated the gateway; one
    /// attempt, and a second 4003 parks again). Not `Repair`/`FixClient`: the same request fails the same way.
    pub fn user_may_reconnect(&self) -> bool {
        matches!(
            self,
            LinkState::Parked {
                reason: CloseAction::MissingScope
                    | CloseAction::AgentRefused
                    | CloseAction::UpgradeClient
            }
        )
    }

    /// What the UI shows (IPC `LinkPhase`).
    pub fn phase(&self) -> fogar_ports::ipc::LinkPhase {
        use fogar_ports::ipc::LinkPhase;
        match self {
            LinkState::Starting | LinkState::Connecting { .. } => LinkPhase::Connecting,
            LinkState::Ready { .. } => LinkPhase::Ready,
            LinkState::Backoff { .. } => LinkPhase::Reconnecting,
            LinkState::NotPaired => LinkPhase::NotPaired,
            LinkState::Parked { .. } => LinkPhase::Parked,
            LinkState::ShuttingDown | LinkState::Stopped => LinkPhase::Stopping,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            LinkState::Starting => "starting",
            LinkState::Connecting { .. } => "connecting",
            LinkState::Ready { .. } => "ready",
            LinkState::Backoff { .. } => "backoff",
            LinkState::NotPaired => "not_paired",
            LinkState::Parked {
                reason: CloseAction::Repair,
            } => "parked_repair",
            LinkState::Parked {
                reason: CloseAction::AgentRefused,
            } => "parked_agent_refused",
            LinkState::Parked {
                reason: CloseAction::MissingScope,
            } => "parked_missing_scope",
            LinkState::Parked {
                reason: CloseAction::UpgradeClient,
            } => "parked_upgrade_client",
            LinkState::Parked { .. } => "parked_client_error",
            LinkState::ShuttingDown => "shutting_down",
            LinkState::Stopped => "stopped",
        }
    }
}

macro_rules! counters {
    ($($name:ident: $doc:literal),* $(,)?) => {
        /// Monotone counters (Relaxed: statistics only).
        #[derive(Debug, Default)]
        pub struct Stats { $(#[doc = $doc] pub $name: AtomicU64,)* }

        /// Point-in-time copy of [`Stats`].
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct StatsSnapshot { $(#[doc = $doc] pub $name: u64,)* }

        impl Stats {
            pub fn snapshot(&self) -> StatsSnapshot {
                StatsSnapshot { $($name: self.$name.load(Ordering::Relaxed),)* }
            }
        }
    };
}

counters! {
    connect_attempts: "transport connects attempted",
    connect_failures: "connects, handshakes or ready-waits that failed",
    sessions_started: "sessions that reached `ready`",
    sessions_ended: "sessions that ended after `ready`",
    keepalives_sent: "`keepalive` messages sent",
    frames_sent: "frames written to the transport",
    stale_outbound_discarded: "uplink frames from a dead session discarded before the next one",
    uplink_audio_shed: "utterance audio chunks shed because the uplink queue was full",
    uplink_control_dropped: "control messages that missed their enqueue deadline",
    inbound_audio_shed: "TTS frames shed because the engine queue was full",
    inbound_invalid: "inbound text frames or `/events` events that failed to decode",
    events_received: "`/events` events the engine accepted (not `ready`, `resync`, audio or unknown)",
    events_resync: "re-fetches asked of the engine (`resync` events and oversize events)",
    events_oversize: "`/events` events over the reader's cap, skipped",
    events_audio_dropped: "push `audio` events dropped by the link (nothing plays in 4a)",
    stream_rotations: "`/events` streams replaced within the grace, invisible above the link",
    rest_calls: "conversation REST calls made by the engine (list, open, history, send, create, cancel, /auth/me)",
    rest_failures: "engine REST calls that failed: transport, deadline, non-2xx or undecodable body",
    rest_busy: "sends, creates or cancels refused because `REST_IN_FLIGHT` writes were in flight",
    writes_missing_scope: "writes answered `403 missing_scope` (later sends are refused without a call)",
    open_overflows: "open/create buffers that overflowed (the conversation is opened again)",
    list_refreshes: "`GET /conversations` asked for (fresh link, resync, unknown conversation)",
    mic_chunks: "capture chunks processed",
    utterances: "utterances detected by the VAD while online",
    turns_started: "turn_start received (any conversation, any device)",
    turns_completed: "turn_end received (any conversation, any device)",
    barge_ins: "turns or playback tails cut by a new text or the cancel button",
    user_waits_timed_out: "turn_start waits abandoned",
    texts_sent: "typed texts posted (`POST …/messages` or `POST /conversations`)",
    cancels_sent: "cancel-button presses posted (`POST …/cancel`)",
    engine_commands_busy: "IPC commands refused because the engine queue was full or slow",
    tts_frames_played: "TTS frames a live output accepted (at least one sample)",
    tts_frames_stale: "TTS frames dropped by turn_seq gating",
    tts_frames_invalid: "binary frames that failed to parse",
    tts_samples_dropped: "TTS samples of a confirmed turn not played: full or dead output, device not ready in time",
    tts_turns_seen: "turns confirmed visible in time: their audio may play (idle until PR 5)",
    tts_turns_muted: "turns whose audio was discarded: not confirmed visible within 2 s, or 3 s waited",
    tts_samples_muted: "TTS samples discarded because their turn was never confirmed visible",
    tts_turns_unplayed: "confirmed turns whose audio was lost before anything played (failed open, no output)",
    playback_opens: "output device opens that succeeded",
    playback_open_failures: "output device opens that failed, timed out or gave a wrong format",
    playback_dead: "open outputs found dead (`is_alive` false) and released (issue #25)",
    playback_idle_closes: "outputs released after draining with no turn open (energy)",
    playback_audio_jobs: "blocking speaker open/release jobs run off the runtime",
    captures_opened: "takes opened by the consent gate (microphone on)",
    captures_refused_unsubscribed: "capture_start from a connection not subscribed to the conversation",
    captures_refused_permission: "takes refused: microphone permission not granted",
    captures_refused_unavailable: "takes refused: no capture support, no device, failed open or open backoff",
    captures_refused_not_ready: "takes refused: link not ready or no live transcriber",
    captures_refused_busy: "takes refused: a take (or its device open/release) in progress",
    captures_silence_cut: "takes cut because nothing new was heard (Review)",
    captures_safety_cut: "takes cut at the safety limit (Review)",
    captures_discarded: "takes ended with Discard (by the person, too short, or their connection left)",
    capture_audio_jobs: "blocking microphone open/release jobs run off the runtime",
    transcriptions_timed_out: "final transcripts that never came after `end`",
    mic_permission_requests: "explicit permission requests passed to the OS",
    ipc_connections: "IPC connections served",
    legacy_secret_delete_failures: "old pairing items (S1 record, slice-0 key) the store failed to delete at login/logout",
    out_queue_hwm: "high-water mark of the uplink queue",
    in_queue_hwm: "high-water mark of the inbound queue",
}

impl Stats {
    pub(crate) fn inc(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn add(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn hwm(c: &AtomicU64, v: usize) {
        c.fetch_max(u64::try_from(v).unwrap_or(u64::MAX), Ordering::Relaxed);
    }
}

/// Snapshot served to the UI and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    pub link: LinkState,
    pub last_close: Option<(u16, CloseAction)>,
    pub stats: StatsSnapshot,
    pub boot_mode: &'static str,
    /// Current depth of the inbound queue (should be ~0 at idle).
    pub in_queue_len: usize,
}
