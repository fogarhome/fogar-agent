//! Slice 0: Unsupported (data, not an error). S8 maps WorkClass to SCHED_IDLE / nice /
//! PR_SET_TIMERSLACK (needs libc, i.e. unsafe FFI in this crate) and documents the systemd unit
//! knobs (CPUWeight=, TimerSlackNSec=). See fogar-energy-budget references/power-policy-port.md.

use fogar_ports::power::{Applied, PowerCaps, PowerError, PowerPolicy, WorkClass};

#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxPowerPolicy;

impl PowerPolicy for LinuxPowerPolicy {
    fn capabilities(&self) -> PowerCaps {
        PowerCaps::NONE
    }
    fn apply_process_baseline(&self) -> Result<Applied, PowerError> {
        Ok(Applied::NotSupported)
    }
    fn apply_current_thread(&self, _class: WorkClass) -> Result<Applied, PowerError> {
        Ok(Applied::NotSupported)
    }
}
