//! Minimal argument parsing (no clap: one less dependency for a 5-flag CLI).

use std::path::PathBuf;

pub(crate) const USAGE: &str = "usage:
  fogar-agentd run [--gateway ws://127.0.0.1:8004/ws/stream] [--agent NAME] [--state-dir DIR] [--keepalive-secs N] [--no-ipc]
  fogar-agentd store-key [--state-dir DIR]   (client key on stdin; dev only: `run --gateway` turns it into a pairing)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunOpts {
    /// Dev path (Linux): pairs a key stored with `store-key` to this address. The app pairs
    /// through the IPC instead.
    pub(crate) gateway: Option<String>,
    pub(crate) agent: Option<String>,
    pub(crate) state_dir: Option<PathBuf>,
    pub(crate) keepalive_secs: Option<u64>,
    pub(crate) no_ipc: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Run(RunOpts),
    StoreKey { state_dir: Option<PathBuf> },
}

pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Command, String> {
    let sub = args.next().ok_or("missing command")?;
    let mut gateway = None;
    let mut agent = None;
    let mut state_dir = None;
    let mut keepalive_secs = None;
    let mut no_ipc = false;
    while let Some(a) = args.next() {
        let mut val = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--gateway" => gateway = Some(val("--gateway")?),
            "--agent" => agent = Some(val("--agent")?),
            "--state-dir" => state_dir = Some(PathBuf::from(val("--state-dir")?)),
            "--keepalive-secs" => {
                keepalive_secs = Some(
                    val("--keepalive-secs")?
                        .parse()
                        .map_err(|_| "--keepalive-secs must be a number")?,
                )
            }
            "--no-ipc" => no_ipc = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    match sub.as_str() {
        "run" => Ok(Command::Run(RunOpts {
            gateway,
            agent,
            state_dir,
            keepalive_secs,
            no_ipc,
        })),
        "store-key" => Ok(Command::StoreKey { state_dir }),
        other => Err(format!("unknown command {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Command, String> {
        parse(s.split_whitespace().map(str::to_owned))
    }

    #[test]
    fn parses() {
        assert_eq!(
            p("run --gateway ws://127.0.0.1:1/ws/stream --agent main --keepalive-secs 0 --no-ipc")
                .unwrap(),
            Command::Run(RunOpts {
                gateway: Some("ws://127.0.0.1:1/ws/stream".into()),
                agent: Some("main".into()),
                state_dir: None,
                keepalive_secs: Some(0),
                no_ipc: true
            })
        );
        assert!(matches!(
            p("run"),
            Ok(Command::Run(RunOpts { gateway: None, .. }))
        ));
        assert!(p("run --gateway").is_err());
        assert!(p("run --gateway x --bogus").is_err());
        assert!(p("").is_err());
        assert_eq!(
            p("store-key --state-dir /x").unwrap(),
            Command::StoreKey {
                state_dir: Some("/x".into())
            }
        );
    }
}
