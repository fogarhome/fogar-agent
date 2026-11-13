//! /proc-based probe: load + memory (`Native`: two small procfs reads, no allocation-heavy work);
//! thermal/battery/idle/events `Unsupported` in slice 0.

use fogar_ports::{
    Support,
    system_probe::{Memory, ProbeCaps, ProbeError, SystemProbe, SystemSnapshot},
};

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcSystemProbe;

fn platform(e: std::io::Error) -> ProbeError {
    ProbeError::Platform(Box::new(e))
}

fn parse_err(what: &'static str) -> ProbeError {
    ProbeError::Platform(what.into())
}

impl SystemProbe for ProcSystemProbe {
    fn capabilities(&self) -> ProbeCaps {
        let mut c = ProbeCaps::NONE;
        c.load = Support::Native;
        c.memory = Support::Native;
        c
    }

    fn snapshot(&self) -> Result<SystemSnapshot, ProbeError> {
        let load = std::fs::read_to_string("/proc/loadavg").map_err(platform)?;
        let load_1m: f32 = load
            .split_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| parse_err("unparsable /proc/loadavg"))?;
        let meminfo = std::fs::read_to_string("/proc/meminfo").map_err(platform)?;
        let kb = |key: &str| -> Option<u64> {
            meminfo
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|rest| {
                    rest.trim()
                        .trim_end_matches("kB")
                        .trim()
                        .parse::<u64>()
                        .ok()
                })
        };
        let total = kb("MemTotal:").ok_or_else(|| parse_err("MemTotal missing"))?;
        let avail = kb("MemAvailable:").ok_or_else(|| parse_err("MemAvailable missing"))?;
        Ok(SystemSnapshot::new()
            .with_load(load_1m.max(0.0))
            .with_memory(Memory {
                total_bytes: total * 1024,
                available_bytes: avail.min(total) * 1024,
            }))
    }
}
