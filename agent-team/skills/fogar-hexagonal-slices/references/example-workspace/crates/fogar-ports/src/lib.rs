//! fogar-ports: the hexagon's edge. Owned by the core team, changed contract-first.
//!
//! Rules for this crate:
//! - no platform cfg (archtest R5 greps for it); no OS crates; no executor; no I/O.
//! - one module per port; each port is small (<= ~5 methods) and has a capability query.
//! - every port has a fake in `fogar-testkit` and a contract suite every adapter must pass.
#![forbid(unsafe_code)]

pub mod clock;
pub mod events;
pub mod support;
pub mod system_probe;
pub mod uplink;

pub use support::Support;
