//! `TailnetPeers` from the Tailscale app: runs its bundled CLI, `Tailscale status --json`, and
//! keeps one tailnet address per online peer of this user. No unsafe, no LocalAPI (the App Store
//! build has no fixed socket path; the CLI finds it).
//!
//! - Absolute path only, never a PATH lookup. Empty environment except `TAILSCALE_BE_CLI=1`:
//!   without it the app binary started outside a shell (no `SHLVL`) runs as the GUI and never
//!   answers (seen on Tailscale 1.104.6; GUESSED for other versions: if a later one ignores the
//!   variable, the call runs into the core's `tailnet_timeout` and the child is killed).
//! - stdin/stderr closed, stdout read with a cap; the child is killed when the future is dropped
//!   (the core's deadline), so an expired call leaves nothing running.
//! - The JSON is semi-trusted input: only the fields used are read, and every address must pass
//!   `is_tailnet_ip`. The core still probes each one through `GatewayHttp`.
//! - Only peers of the SAME Tailscale user as this machine (`UserID` == `Self.UserID`): other
//!   people's devices in a shared tailnet, or nodes shared in from elsewhere, could otherwise pose
//!   as "green-house" and relay the pairing code (security review S-1).
//!   shortcut: tagged nodes (owned by the tailnet, another `UserID`) are left out; type their
//!   address by hand, or allow tags here if a gateway ever runs on one.

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
    #[serde(rename = "Self")]
    me: Option<Node>,
    /// Keyed by node key; this machine is under `Self`, never here.
    #[serde(rename = "Peer", default)]
    peer: Option<BTreeMap<String, Node>>,
}

#[derive(Deserialize)]
struct Node {
    #[serde(rename = "UserID", default)]
    user_id: Option<u64>,
    #[serde(rename = "TailscaleIPs", default)]
    ips: Option<Vec<IpAddr>>,
    #[serde(rename = "Online", default)]
    online: bool,
}

/// One address per online peer of this user (IPv4 preferred), tailnet ranges only, unique,
/// capped.
fn parse_status(json: &[u8]) -> Result<Vec<IpAddr>, TailnetError> {
    let s: Status = serde_json::from_slice(json).map_err(|_| TailnetError::Malformed)?;
    if s.backend_state != "Running" {
        return Err(TailnetError::NotRunning);
    }
    // Without our own user we cannot tell whose peers they are.
    let me =
        s.me.and_then(|n| n.user_id)
            .ok_or(TailnetError::Malformed)?;
    let mine = |p: &Node| p.online && p.user_id == Some(me);
    let mut out: Vec<IpAddr> = Vec::new();
    for p in s.peer.unwrap_or_default().into_values().filter(mine) {
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
    use std::time::Duration;

    fn ips(v: &[&str]) -> Vec<IpAddr> {
        v.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn keeps_online_peers_of_this_user_ipv4_first() {
        let json = br#"{"BackendState":"Running","Self":{"UserID":7,"TailscaleIPs":["100.89.15.7"]},
          "Peer":{
            "a":{"UserID":7,"HostName":"green-house","TailscaleIPs":["100.109.57.116","fd7a:115c:a1e0::2e3b:3976"],"Online":true},
            "b":{"UserID":7,"TailscaleIPs":["100.126.18.95"],"Online":false},
            "c":{"UserID":7,"TailscaleIPs":["fd7a:115c:a1e0::7e3b:b136"],"Online":true},
            "d":{"UserID":7,"TailscaleIPs":["8.8.8.8","192.168.1.19"],"Online":true},
            "e":{"UserID":7,"TailscaleIPs":["100.109.57.116"],"Online":true},
            "f":{"UserID":7,"Online":true},
            "g":{"UserID":8,"HostName":"green-house","TailscaleIPs":["100.100.1.1"],"Online":true},
            "h":{"TailscaleIPs":["100.100.1.2"],"Online":true}
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
            parse_status(br#"{"BackendState":"Running","Self":{"UserID":7}}"#).unwrap(),
            Vec::<IpAddr>::new()
        );
        assert_eq!(
            parse_status(br#"{"BackendState":"Running"}"#),
            Err(TailnetError::Malformed)
        );
    }

    #[test]
    fn caps_at_max_peers() {
        let peers: Vec<String> = (0..100)
            .map(|i| {
                format!(r#""k{i:03}":{{"UserID":1,"TailscaleIPs":["100.64.0.{i}"],"Online":true}}"#)
            })
            .collect();
        let json = format!(
            r#"{{"BackendState":"Running","Self":{{"UserID":1}},"Peer":{{{}}}}}"#,
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

    /// A stand-in CLI: a `#!/bin/sh` script in its own temp dir.
    fn stand_in(tag: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("fogar-ts-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cli = dir.join("Tailscale");
        std::fs::write(&cli, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        cli
    }

    #[tokio::test]
    async fn stand_in_cli_answers_in_cli_mode_with_an_empty_env() {
        let cli = stand_in(
            "ok",
            r#"[ "$TAILSCALE_BE_CLI" = 1 ] && [ -z "$HOME" ] && [ "$1 $2" = "status --json" ] || exit 3
echo '{"BackendState":"Running","Self":{"UserID":1},"Peer":{"a":{"UserID":1,"TailscaleIPs":["100.109.57.116"],"Online":true}}}'"#,
        );
        let t = MacTailnet::with_cli(&cli);
        assert_eq!(t.peers().await, Ok(ips(&["100.109.57.116"])));
    }

    #[tokio::test]
    async fn oversized_output_is_malformed() {
        let cli = stand_in("big", "head -c 1100000 /dev/zero; exec sleep 30");
        let t = MacTailnet::with_cli(&cli);
        let r = tokio::time::timeout(Duration::from_secs(10), t.peers()).await;
        assert_eq!(r.unwrap(), Err(TailnetError::Malformed));
    }

    #[tokio::test]
    async fn dropped_call_kills_the_cli() {
        let pid_file = std::env::temp_dir().join(format!("fogar-ts-pid-{}", std::process::id()));
        let _ = std::fs::remove_file(&pid_file);
        let cli = stand_in(
            "hang",
            &format!("echo $$ > '{}'; exec sleep 30", pid_file.display()),
        );
        let t = MacTailnet::with_cli(&cli);
        // Drop the call once the stand-in runs (a new script can take a while to start:
        // macOS assesses fresh executables).
        let started = async {
            while !pid_file.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::select! {
            r = t.peers() => panic!("the stand-in never answers: {r:?}"),
            () = started => {}
            () = tokio::time::sleep(Duration::from_secs(10)) => panic!("stand-in did not start"),
        }
        tokio::time::sleep(Duration::from_millis(50)).await; // `$$` written, `exec sleep` done
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let alive = || {
            std::process::Command::new("/bin/kill")
                .args(["-0", pid.trim()])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        for _ in 0..50 {
            if !alive() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the CLI outlived the dropped call");
    }
}
