//! Engine configuration, injected by the composition root (core never reads env or files).

use crate::{backoff::BackoffPolicy, turn::TurnTimeouts, vad::VadConfig};
use fogar_ports::gateway::GatewayAddr;
use fogar_proto::{InputMode, OutputMode, limits};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Gateway to connect to. `None` = not paired (S1 core PR replaces this with the pairing
    /// record from the SecretStore).
    pub gateway: Option<GatewayAddr>,
    /// Requested agent; `None` = gateway fallback cascade (spec §2).
    pub agent: Option<String>,
    pub input_mode: InputMode,
    /// We never request `status` (one `pipeline_event` per STT partial; spec §7).
    pub output_mode: Vec<OutputMode>,
    pub connect_timeout: Duration,
    /// Handshake sent -> `ready` received (the gateway's own handshake limit is 5 s).
    pub ready_timeout: Duration,
    /// Each transport write and each enqueue of a must-deliver uplink message.
    pub write_timeout: Duration,
    /// Graceful close handshake on shutdown. Must stay below half the shutdown budget (the egress
    /// phase, see `Agent::shutdown`): a hung peer must not turn an ordinary stop into "unclean".
    pub close_timeout: Duration,
    /// Session -> engine enqueue deadline for JSON events; must stay below the gateway's
    /// CLIENT_SEND_TIMEOUT_S (10 s) or the gateway closes us with 1013.
    pub inbound_deadline: Duration,
    /// Send `keepalive` after this much client silence; `None` disables it (tests of the idle
    /// close). The gateway closes idle sessions after 300 s with 1000 and no warning.
    pub keepalive_after: Option<Duration>,
    pub backoff: BackoffPolicy,
    /// A local UI connection that sends no complete request for this long is closed, so silent
    /// clients cannot hold the `IPC_MAX_CONNS` slots (review code#3/sec#5).
    pub ipc_idle_timeout: Duration,
    pub vad: VadConfig,
    pub turn: TurnTimeouts,
    /// Capture rate the engine streams as-is (f32le 16 kHz mono is all v1 accepts). Captures at
    /// another rate are refused in slice 0 (resampling arrives with S2).
    pub upload_rate_hz: u32,
    /// Liveness heartbeat period (OS-thread watchdog reads it). Coarse on purpose: energy budget.
    pub heartbeat_every: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            gateway: None,
            agent: None,
            input_mode: InputMode::Audio,
            output_mode: vec![OutputMode::Audio, OutputMode::Text],
            connect_timeout: Duration::from_secs(10),
            ready_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(5),
            close_timeout: Duration::from_secs(1),
            inbound_deadline: Duration::from_secs(5),
            keepalive_after: Some(Duration::from_secs(u64::from(limits::KEEPALIVE_EVERY_S))),
            backoff: BackoffPolicy::default(),
            ipc_idle_timeout: Duration::from_secs(30),
            vad: VadConfig::default(),
            turn: TurnTimeouts::default(),
            upload_rate_hz: fogar_proto::audio::UPLOAD_RATE_HZ,
            heartbeat_every: Duration::from_secs(10),
        }
    }
}
