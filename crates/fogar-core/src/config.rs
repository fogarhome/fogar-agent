//! Engine configuration, injected by the composition root (core never reads env or files).

use crate::{backoff::BackoffPolicy, vad::VadConfig};
use fogar_proto::{InputMode, OutputMode, limits};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// This device's name at sign-in (`POST /auth/login` `device_name`).
    /// The composition root sets it (e.g. the Mac's name).
    pub client_name: String,
    /// Deadline for probing one address (`/.well-known`, both paths; spec §0: 1–2 s).
    pub probe_timeout: Duration,
    /// Deadline for asking the tailnet for its peers (discovery, and the sign-in address check).
    pub tailnet_timeout: Duration,
    /// Deadline for `POST /auth/login` and `POST /auth/logout`.
    pub account_timeout: Duration,
    /// Answer `Login`/`Logout` from a peer the IPC adapter only checked by OS user (IPC-4).
    /// `false` = only `PeerCheck::SameSigner`. The composition root may set it ONLY from a
    /// compile-time switch of development builds, never from a runtime flag.
    pub pair_from_uid_only_peers: bool,
    /// Allow signing in to this machine (a loopback literal or `localhost`) over clear text.
    /// `false` = refused with `NeedsSecureAddress`: another local user can listen on
    /// 127.0.0.1:8004 and receive the password and the token. Development and tests only; the
    /// composition root may set it ONLY from a compile-time switch of development builds.
    pub login_over_loopback: bool,
    /// Requested agent; `None` = gateway fallback cascade (spec §2).
    pub agent: Option<String>,
    pub input_mode: InputMode,
    /// `status` is requested for `turn_progress` and the voice services' `status` (S2); it also
    /// brings one `pipeline_event` per STT partial (spec §7), which the engine ignores.
    pub output_mode: Vec<OutputMode>,
    /// Opening `/events` (dial, request, response head).
    pub connect_timeout: Duration,
    /// Stream opened -> its `ready` event received.
    pub ready_timeout: Duration,
    /// No byte at all on `/events` (not even `: ping`, sent every 15 s) for this long: the
    /// stream is dead, reconnect.
    pub events_read_timeout: Duration,
    /// An `/events` stream that lasted this long and ended cleanly was a planned rotation (the
    /// gateway ends each one after ~25 s): reopen at once. Shorter streams back off. GUESSED.
    pub stable_stream: Duration,
    /// A planned rotation that reaches `ready` again within this is invisible above the link;
    /// a slower one is a real `Down`. GUESSED.
    pub rotation_grace: Duration,
    /// Each REST call to the gateway, and reading a non-2xx body.
    pub http_timeout: Duration,
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
    /// Capture rate the engine streams as-is (f32le 16 kHz mono is all v1 accepts). The adapter
    /// resamples; a capture delivered at another format is closed and the take refused.
    pub upload_rate_hz: u32,
    /// A take is cut (outcome `Review`, never sent) when no new non-empty
    /// `transcription_partial` arrived for this long. Gateway 5.0's silence watchdog only warns
    /// and never closes, so this is the only guard there. A 4.x gateway still closes at ~6 s with
    /// its defaults (`silence_timeout_s` 2 s x `max_silence_turns` 3); 5 s beats it by ~1 s, but an
    /// admin's per-client value below 5 s on 4.x can make the gateway close first (spec S2
    /// «Límites de una toma»).
    pub capture_silence_cut: Duration,
    /// Privacy net for a forgotten take that keeps hearing something (a TV): cut with `Review`.
    pub capture_safety_cut: Duration,
    /// A take shorter than this is discarded whatever the person asked (a slip of the finger).
    pub capture_min: Duration,
    /// How long an IPC `RequestMicPermission` waits for the person to answer the system prompt
    /// before it is answered with the current status.
    pub mic_permission_wait: Duration,
    /// Liveness heartbeat period (OS-thread watchdog reads it). Coarse on purpose: energy budget.
    pub heartbeat_every: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // GUESSED: the composition root should pass the device name.
            client_name: "Fogar Desktop".into(),
            probe_timeout: Duration::from_millis(1500),
            // The Tailscale CLI answers in ~0.1 s; discovery as a whole must stay well under the
            // app's 10 s `discover` timeout (tailnet_timeout + probe_timeout).
            tailnet_timeout: Duration::from_secs(2),
            account_timeout: Duration::from_secs(10),
            pair_from_uid_only_peers: false,
            login_over_loopback: false,
            agent: None,
            input_mode: InputMode::Audio,
            output_mode: vec![OutputMode::Audio, OutputMode::Text, OutputMode::Status],
            connect_timeout: Duration::from_secs(10),
            ready_timeout: Duration::from_secs(10),
            events_read_timeout: Duration::from_secs(40),
            stable_stream: Duration::from_secs(5),
            rotation_grace: Duration::from_secs(3),
            http_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(5),
            close_timeout: Duration::from_secs(1),
            inbound_deadline: Duration::from_secs(5),
            keepalive_after: Some(Duration::from_secs(u64::from(limits::KEEPALIVE_EVERY_S))),
            backoff: BackoffPolicy::default(),
            ipc_idle_timeout: Duration::from_secs(30),
            vad: VadConfig::default(),
            upload_rate_hz: fogar_proto::audio::UPLOAD_RATE_HZ,
            heartbeat_every: Duration::from_secs(10),
            capture_silence_cut: Duration::from_secs(5),
            capture_safety_cut: Duration::from_secs(5 * 60),
            capture_min: Duration::from_millis(250),
            mic_permission_wait: Duration::from_secs(60),
        }
    }
}
