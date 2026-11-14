//! Linux adapters (illustrative). Whole crate is empty on non-Linux hosts.
#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

use fogar_ports::{
    Support,
    system_probe::{Memory, ProbeCaps, ProbeError, SystemProbe, SystemSnapshot},
};

#[derive(Debug, Default)]
pub struct ProcfsProbe;

impl ProcfsProbe {
    pub fn new() -> Result<Self, ProbeError> {
        std::fs::metadata("/proc/loadavg").map_err(|e| ProbeError::Platform(Box::new(e)))?;
        Ok(Self)
    }
}

fn read(path: &str) -> Result<String, ProbeError> {
    std::fs::read_to_string(path).map_err(|e| ProbeError::Platform(Box::new(e)))
}

fn meminfo_kib(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
}

impl SystemProbe for ProcfsProbe {
    fn capabilities(&self) -> ProbeCaps {
        let mut c = ProbeCaps::NONE;
        c.load = Support::Native;
        c.memory = Support::Native;
        c // thermal/battery/idle: Unsupported in this sketch (would be sysfs / UPower / logind)
    }

    fn snapshot(&self) -> Result<SystemSnapshot, ProbeError> {
        let load = read("/proc/loadavg")?
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<f32>().ok())
            .ok_or(ProbeError::Unsupported("unparseable /proc/loadavg"))?;
        let mi = read("/proc/meminfo")?;
        let mut s = SystemSnapshot::new().with_load(load);
        if let (Some(t), Some(a)) = (
            meminfo_kib(&mi, "MemTotal:"),
            meminfo_kib(&mi, "MemAvailable:"),
        ) {
            s = s.with_memory(Memory {
                total_bytes: t * 1024,
                available_bytes: a * 1024,
            });
        }
        Ok(s)
    }
}
