//! `TailnetPeers` from the Tailscale app: runs its bundled CLI, `Tailscale status --json`, and
//! keeps one tailnet address per online peer. No unsafe, no LocalAPI (the App Store build has no
//! fixed socket path; the CLI finds it).
//!
//! - Absolute path only, never a PATH lookup. Empty environment except `TAILSCALE_BE_CLI=1`:
//!   without it the app binary started outside a shell (no `SHLVL`) runs as the GUI and never
//!   answers (seen on Tailscale 1.104.6).
//! - stdin/stderr closed, stdout read with a cap; the child is killed when the future is dropped
//!   (the core's deadline), so an expired call leaves nothing running.
//! - The JSON is semi-trusted input: only the fields used are read, and every address must pass
//!   `is_tailnet_ip`. The core still probes each one through `GatewayHttp`.

use fogar_ports::{
    Support,
    tailnet::{MAX_TAILNET_PEERS, TailnetError, TailnetPeers, is_tailnet_ip},
};
use serde::Deserialize;
use std::{collections::BTreeMap, net::IpAddr, path::PathBuf, process::Stdio};
use tokio::{io::AsyncReadExt, process::Command};

/// The app's bundled CLI (App Store and standalone builds both install here).
/// shortcut: no Homebrew `tailscale`/`tailscaled` paths, add them if someone uses that setup.
const APP_CLI: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";
/// `status --json` is ~7 KB for 3 peers; 1 MiB is thousands of peers.
const MAX_STATUS_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct MacTailnet {
    cli: PathBuf,
}

impl Default for MacTailnet {
    fn default() -> Self {
        Self::with_cli(APP_CLI)
    }
}

impl MacTailnet {
    /// Tests: a stand-in CLI.
    pub fn with_cli(cli: impl Into<PathBuf>) -> Self {
        Self { cli: cli.into() }
    }
}

impl TailnetPeers for MacTailnet {
    /// Native on macOS; whether the app is installed is checked per call (it can come later).
    fn capabilities(&self) -> Support {
        Support::Native
    }

    async fn peers(&self) -> Result<Vec<IpAddr>, TailnetError> {
        if !self.cli.is_file() {
            return Err(TailnetError::Unavailable);
        }
        let mut child = Command::new(&self.cli)
            .args(["status", "--json"])
            .env_clear()
            .env("TAILSCALE_BE_CLI", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| TailnetError::Unavailable)?;
        let mut out = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            stdout
                .take(MAX_STATUS_BYTES + 1)
                .read_to_end(&mut out)
                .await
                .map_err(|_| TailnetError::Malformed)?;
        }
        if out.len() as u64 > MAX_STATUS_BYTES {
            return Err(TailnetError::Malformed); // dropping `child` kills it
        }
        let status = child.wait().await.map_err(|_| TailnetError::Malformed)?;
        if !status.success() {
            // The CLI exits non-zero when it cannot reach the Tailscale service.
            return Err(TailnetError::NotRunning);
        }
        parse_status(&out)
    }
}

#[derive(Deserialize)]
struct Status {
    #[serde(rename = "BackendState")]
    backend_state: String,
    /// Keyed by node key; this machine is under `Self`, never here.
    #[serde(rename = "Peer", default)]
    peer: Option<BTreeMap<String, Peer>>,
}

#[derive(Deserialize)]
struct Peer {
    #[serde(rename = "TailscaleIPs", default)]
    ips: Option<Vec<IpAddr>>,
    #[serde(rename = "Online", default)]
    online: bool,
}

/// One address per online peer (IPv4 preferred), tailnet ranges only, unique, capped.
fn parse_status(json: &[u8]) -> Result<Vec<IpAddr>, TailnetError> {
    let s: Status = serde_json::from_slice(json).map_err(|_| TailnetError::Malformed)?;
    if s.backend_state != "Running" {
        return Err(TailnetError::NotRunning);
    }
    let mut out: Vec<IpAddr> = Vec::new();
    for p in s
        .peer
        .unwrap_or_default()
        .into_values()
        .filter(|p| p.online)
    {
        let ips: Vec<IpAddr> = p
            .ips
            .unwrap_or_default()
            .into_iter()
            .filter(|ip| is_tailnet_ip(*ip))
            .collect();
        let pick = ips.iter().find(|ip| ip.is_ipv4()).or(ips.first());
        if let Some(&ip) = pick
            && !out.contains(&ip)
        {
            out.push(ip);
            if out.len() == MAX_TAILNET_PEERS {
                break;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn ips(v: &[&str]) -> Vec<IpAddr> {
        v.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn keeps_online_peers_ipv4_first() {
        let json = br#"{"BackendState":"Running","Self":{"TailscaleIPs":["100.89.15.7"]},
          "Peer":{
            "a":{"HostName":"green-house","TailscaleIPs":["100.109.57.116","fd7a:115c:a1e0::2e3b:3976"],"Online":true},
            "b":{"TailscaleIPs":["100.126.18.95"],"Online":false},
            "c":{"TailscaleIPs":["fd7a:115c:a1e0::7e3b:b136"],"Online":true},
            "d":{"TailscaleIPs":["8.8.8.8","192.168.1.19"],"Online":true},
            "e":{"TailscaleIPs":["100.109.57.116"],"Online":true},
            "f":{"Online":true}
          }}"#;
        assert_eq!(
            parse_status(json).unwrap(),
            ips(&["100.109.57.116", "fd7a:115c:a1e0::7e3b:b136"])
        );
    }

    #[test]
    fn not_running_and_garbage() {
        let stopped = br#"{"BackendState":"Stopped","Peer":null}"#;
        assert_eq!(parse_status(stopped), Err(TailnetError::NotRunning));
        assert_eq!(parse_status(b"nope"), Err(TailnetError::Malformed));
        assert_eq!(
            parse_status(br#"{"BackendState":"Running"}"#).unwrap(),
            Vec::<IpAddr>::new()
        );
    }

    #[test]
    fn caps_at_max_peers() {
        let peers: Vec<String> = (0..100)
            .map(|i| format!(r#""k{i:03}":{{"TailscaleIPs":["100.64.0.{i}"],"Online":true}}"#))
            .collect();
        let json = format!(
            r#"{{"BackendState":"Running","Peer":{{{}}}}}"#,
            peers.join(",")
        );
        assert_eq!(
            parse_status(json.as_bytes()).unwrap().len(),
            MAX_TAILNET_PEERS
        );
    }

    #[tokio::test]
    async fn missing_cli_is_unavailable() {
        let t = MacTailnet::with_cli("/nonexistent/Tailscale");
        assert_eq!(t.peers().await, Err(TailnetError::Unavailable));
    }

    #[tokio::test]
    async fn failing_cli_is_not_running() {
        let t = MacTailnet::with_cli("/usr/bin/false");
        assert_eq!(t.peers().await, Err(TailnetError::NotRunning));
    }
}
