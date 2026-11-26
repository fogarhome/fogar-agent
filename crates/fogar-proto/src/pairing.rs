//! Discovery REST bodies and the client type (spec §3). The 4.x pairing bodies (`POST /pair`)
//! are gone: 5.0 signs in with an account (`crate::account`). HTTP itself lives in an adapter.

use alloc::string::String;
use serde::{Deserialize, Serialize};

/// CERTAIN. Probe both paths during the jota -> fogar rename (v2.0.0, PR open).
pub const WELL_KNOWN_PATHS: [&str; 2] = ["/.well-known/fogar-gateway", "/.well-known/jota-gateway"];

/// CERTAIN. `service` is "jota-gateway" or "fogar-gateway".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WellKnown {
    pub service: String,
    pub version: String,
    /// CERTAIN (5.0, #417). Gone in 5.0 (the home name arrives at login); a 4.x gateway still sends it.
    #[serde(default)]
    pub name: Option<String>,
}

/// CERTAIN values. No desktop-agent type exists yet (spec §17 gap): use `App` until it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientType {
    Ha,
    Esp32,
    Web,
    App,
    Screen,
}
