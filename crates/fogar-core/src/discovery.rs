//! Gateway discovery (client-protocol §0): the `/.well-known` probe and the candidates of the
//! saved address, this machine and the tailnet peers. HTTP is the `GatewayHttp` port; the
//! well-known body comes from fogar-proto. Every probe runs under `Config::probe_timeout`.

use crate::{
    account::{Account, load},
    time::deadline,
};
use fogar_ports::{
    clock::Clock,
    gateway::{DEFAULT_GATEWAY_PORT, GatewayAddr},
    gateway_http::{GatewayHttp, HttpCall, HttpError},
    ipc::GatewayCandidate,
    tailnet::{MAX_TAILNET_PEERS, TailnetPeers},
};
use fogar_proto::pairing::WellKnown;
use std::net::SocketAddr;
use tokio::task::JoinSet;

/// `service` values of a Fogar gateway (before and after the jota -> fogar rename).
const FOGAR_SERVICES: [&str; 2] = ["fogar-gateway", "jota-gateway"];
/// Same-machine gateway (spec §0 step 2).
const LOCAL_GATEWAY: &str = "127.0.0.1:8004";
/// Longest gateway-supplied label we keep (names, versions, ids).
const MAX_LABEL_CHARS: usize = 128;

/// Gateway-supplied text shown in the UI and stored in the account: no control, bidi or
/// invisible format characters (UI spoofing), at most `MAX_LABEL_CHARS`.
pub(crate) fn label(s: String) -> String {
    let bidi = |c: char| {
        matches!(c, '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
    };
    if s.chars().count() <= MAX_LABEL_CHARS && !s.chars().any(|c| c.is_control() || bidi(c)) {
        return s;
    }
    s.chars()
        .filter(|&c| !c.is_control() && !bidi(c))
        .take(MAX_LABEL_CHARS)
        .collect()
}

pub(crate) enum Probe {
    Fogar(WellKnown),
    /// Something answered, but not a Fogar gateway.
    NotFogar,
    Unreachable,
    /// The transport policy refuses this address (clear text to a public IP, no TLS).
    Refused,
}

impl<C: Clock, H: GatewayHttp, T: TailnetPeers> Account<C, H, T> {
    /// `/.well-known/fogar-gateway`, then the pre-rename path; the whole probe of one address
    /// is bounded by `probe_timeout`.
    pub(crate) async fn probe(&self, addr: &GatewayAddr) -> Probe {
        let calls = async {
            let mut answered = false;
            for call in &[HttpCall::WellKnown, HttpCall::WellKnownLegacy] {
                match self.http.request(addr, call, None, None).await {
                    Ok(r) if r.status == 200 => {
                        match serde_json::from_slice::<WellKnown>(&r.body) {
                            Ok(wk) if FOGAR_SERVICES.contains(&wk.service.as_str()) => {
                                return Probe::Fogar(wk);
                            }
                            _ => answered = true,
                        }
                    }
                    Ok(_) | Err(HttpError::TooLarge { .. } | HttpError::Transport(_)) => {
                        answered = true;
                    }
                    Err(HttpError::Refused) => return Probe::Refused,
                    // Nothing listens: the legacy path will not answer either.
                    Err(_) => break,
                }
            }
            if answered {
                Probe::NotFogar
            } else {
                Probe::Unreachable
            }
        };
        deadline(&*self.clock, self.cfg.probe_timeout, calls)
            .await
            .unwrap_or(Probe::Unreachable)
    }

    /// Saved address, this machine, then one address per tailnet peer (spec §0 steps 1–3;
    /// manual entry is the UI's). The tailnet is asked only for a `SameSigner` app
    /// (`with_tailnet`): only it may sign in, and it keeps uid-only peers from making the agent
    /// run a child process. Every address is probed at once (at most `MAX_TAILNET_PEERS + 2`
    /// probes, see `queues`); the first two start before the tailnet answers, so the whole call
    /// takes at most `tailnet_timeout + probe_timeout`. Every Fogar that answers is returned,
    /// in that order.
    pub(crate) async fn discover(&self, with_tailnet: bool) -> Vec<GatewayCandidate> {
        let saved = load(&*self.secrets)
            .ok()
            .flatten()
            .map(|r| r.address().clone());
        let local = GatewayAddr::parse(LOCAL_GATEWAY).ok();
        // Dropped with this future (agent shutdown): aborts every probe still running.
        let mut probes = JoinSet::new();
        let mut addrs: Vec<GatewayAddr> = Vec::with_capacity(MAX_TAILNET_PEERS + 2);
        let mut start = |addr: GatewayAddr, probes: &mut JoinSet<_>| {
            if addrs.contains(&addr) {
                return;
            }
            let (i, me, a) = (addrs.len(), self.clone(), addr.clone());
            addrs.push(addr);
            probes.spawn(async move {
                let probe = me.probe(&a).await;
                (i, a, probe)
            });
        };
        for addr in saved.into_iter().chain(local) {
            start(addr, &mut probes);
        }
        if with_tailnet && self.tailnet.capabilities().is_available() {
            let peers = deadline(&*self.clock, self.cfg.tailnet_timeout, self.tailnet.peers())
                .await
                .and_then(Result::ok)
                .unwrap_or_default();
            // The adapter promises the cap; the core does not rely on it.
            for ip in peers.into_iter().take(MAX_TAILNET_PEERS) {
                let addr = SocketAddr::new(ip, DEFAULT_GATEWAY_PORT).to_string();
                if let Ok(addr) = GatewayAddr::parse(&addr) {
                    start(addr, &mut probes);
                }
            }
        }
        let mut found = Vec::new();
        while let Some(r) = probes.join_next().await {
            if let Ok((i, addr, Probe::Fogar(wk))) = r {
                found.push((
                    i,
                    GatewayCandidate::new(
                        addr.to_string(),
                        // 5.0 dropped `name` from the well-known: show the host as probed.
                        label(wk.name.unwrap_or_else(|| addr.host().to_owned())),
                        label(wk.version),
                    ),
                ));
            }
        }
        found.sort_by_key(|(i, _)| *i);
        found.into_iter().map(|(_, c)| c).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_labels_are_cleaned_and_capped() {
        assert_eq!(label("green-house".into()), "green-house");
        assert_eq!(label("a\u{202E}b\nc\u{0}".into()), "abc");
        assert_eq!(
            label("green\u{200B}-house\u{FEFF}\u{2060}".into()),
            "green-house"
        );
        assert_eq!(label("ñ".repeat(500)).chars().count(), MAX_LABEL_CHARS);
    }
}
