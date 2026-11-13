//! Close classification (spec §11). The gateway never sends an `error` before closing, so the
//! client classifies by close code + reason string. Mapping CERTAIN for the listed reasons; the
//! policy (what to do) is ours.

/// What the agent should do after the gateway closed the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloseAction {
    /// Key invalid, inactive or revoked: delete the stored key and re-pair. Never auto-retry.
    Repair,
    /// Agent not permitted or unavailable: keep the key; surface it. No retry loop.
    AgentRefused,
    /// Bug on our side (bad handshake, oversize): do not loop.
    FixClient,
    /// Idle (1000): reconnect lazily, not an error.
    IdleOrNormal,
    /// 1011 critical service, 1013 slow, 1006: reconnect with backoff.
    RetryBackoff,
    /// 1012 server restart: reconnect soon.
    ServerRestart,
    /// Anything else. Policy in fogar-core: treat like `RetryBackoff`.
    Unknown,
}

/// Abnormal closure (no close frame). Reported by transports when the socket just dies.
pub const CLOSE_ABNORMAL: u16 = 1006;
/// Normal closure; what the agent sends on shutdown.
pub const CLOSE_NORMAL: u16 = 1000;

pub fn classify_close(code: u16, reason: &str) -> CloseAction {
    match code {
        1008 => {
            if reason == "revoked" || reason.starts_with("Clave de cliente invalida") {
                CloseAction::Repair
            } else if reason.starts_with("Agent '") {
                CloseAction::AgentRefused
            } else if reason.starts_with("Handshake timeout") {
                CloseAction::RetryBackoff
            } else if reason.starts_with("Handshake") {
                CloseAction::FixClient
            } else {
                CloseAction::Unknown
            }
        }
        1009 => CloseAction::FixClient,
        1000 => CloseAction::IdleOrNormal,
        1011 | 1013 | 1006 => CloseAction::RetryBackoff,
        1012 => CloseAction::ServerRestart,
        _ => CloseAction::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_table() {
        let cases = [
            (1008, "Handshake timeout", CloseAction::RetryBackoff),
            (1008, "Handshake too large", CloseAction::FixClient),
            (
                1008,
                "Handshake invalido. Se esperaba JSON de configuración.",
                CloseAction::FixClient,
            ),
            (
                1008,
                "Handshake invalido. Se esperaba texto.",
                CloseAction::FixClient,
            ),
            (
                1008,
                "Clave de cliente invalida o inactiva.",
                CloseAction::Repair,
            ),
            (
                1008,
                "Agent 'x' not permitted for this client.",
                CloseAction::AgentRefused,
            ),
            (1008, "Agent 'x' not available.", CloseAction::AgentRefused),
            (1008, "revoked", CloseAction::Repair),
            (1008, "something new", CloseAction::Unknown),
            (
                1011,
                "Servicio crítico no disponible.",
                CloseAction::RetryBackoff,
            ),
            (1013, "client too slow", CloseAction::RetryBackoff),
            (1006, "", CloseAction::RetryBackoff),
            (1000, "", CloseAction::IdleOrNormal),
            (1012, "", CloseAction::ServerRestart),
            (1009, "", CloseAction::FixClient),
            (4000, "", CloseAction::Unknown),
        ];
        for (code, reason, want) in cases {
            assert_eq!(classify_close(code, reason), want, "{code} {reason}");
        }
    }
}
