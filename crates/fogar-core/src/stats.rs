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
    /// No client key stored: the agent is not paired. Re-checked at the backoff cap.
    NotPaired,
    /// Parked: retrying would loop (key revoked/invalid, agent refused, client bug). Stays alive
    /// and reports health; leaves this state only on restart or (S1) re-pairing/config change.
    Parked {
        reason: CloseAction,
    },
    ShuttingDown,
    Stopped,
}

impl LinkState {
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
    inbound_invalid: "inbound text frames that failed to decode",
    mic_chunks: "capture chunks processed",
    utterances: "utterances detected by the VAD while online",
    turns_started: "turn_start received",
    turns_completed: "our turns that reached turn_end",
    barge_ins: "local barge-ins",
    user_waits_timed_out: "transcription / turn_start waits abandoned",
    tts_frames_played: "TTS frames handed to playback",
    tts_frames_stale: "TTS frames dropped by turn_seq gating",
    tts_frames_invalid: "binary frames that failed to parse",
    tts_samples_dropped: "TTS samples refused by a full playback buffer",
    ipc_connections: "IPC connections served",
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
    /// Current depth of the uplink / inbound queues (should be ~0 at idle).
    pub out_queue_len: usize,
    pub in_queue_len: usize,
}
