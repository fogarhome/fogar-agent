//! fogar-core: the engine. Pure policy (backoff, VAD, turn state machine, crash-loop breaker) plus
//! orchestration over ports (link supervisor, engine, IPC service, ordered shutdown).
//!
//! **Architecture invariants** (mechanically enforced: archtest R1/R4/R5, clippy.toml):
//! - no OS APIs, no platform cfg, no ambient time; time only via the `Clock` port;
//! - every task is spawned through an owned `TaskTracker` (one per shutdown phase) or a drained
//!   `JoinSet`, and takes a `CancellationToken`;
//! - every queue is bounded and has a written full-policy ([`queues`]).
#![forbid(unsafe_code)]

pub mod account;
pub mod agent;
pub mod backoff;
pub mod config;
pub mod conversation;
pub mod crashloop;
pub mod heartbeat;
pub mod queues;
pub mod stats;
pub mod turn;
pub mod vad;

mod capture;
mod discovery;
mod engine;
mod ipc;
mod link;
mod playback;
mod time;

pub use account::AccountRecord;
pub use agent::{Agent, AgentPorts, HealthProbe, NoHttp, NoIpc, NoTailnet, ShutdownReport};
pub use config::Config;
pub use link::LinkView;
pub use stats::{Health, LinkState, StatsSnapshot};
