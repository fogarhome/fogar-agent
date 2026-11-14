//! macOS SystemProbe - walking-skeleton version: load average only, everything else
//! honestly `Unsupported` until its slice lands. (Not compile-checked on Linux.)
//!
//! Later slices fill in, each with contract tests on the macOS runner:
//! - thermal: `NSProcessInfo.thermalState` + `NSProcessInfoThermalStateDidChangeNotification` (objc2-foundation)
//! - battery / low power: IOKit power sources, `NSProcessInfo.isLowPowerModeEnabled`
//! - idle: IOKit HIDIdleTime
//! - memory: `host_statistics64` (vm_statistics64) + sysctl `hw.memsize`

use fogar_ports::{
    Support,
    system_probe::{ProbeCaps, ProbeError, SystemProbe, SystemSnapshot},
};

#[derive(Debug, Default)]
pub struct MacSystemProbe;

impl MacSystemProbe {
    pub fn new() -> Result<Self, ProbeError> {
        Ok(Self)
    }
}

impl SystemProbe for MacSystemProbe {
    fn capabilities(&self) -> ProbeCaps {
        let mut c = ProbeCaps::NONE;
        c.load = Support::Native;
        c
    }

    fn snapshot(&self) -> Result<SystemSnapshot, ProbeError> {
        let mut avg = [0f64; 3];
        // SAFETY: getloadavg writes at most `nelem` doubles into the buffer.
        let n = unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) };
        if n < 1 {
            return Err(ProbeError::Platform(Box::new(
                std::io::Error::last_os_error(),
            )));
        }
        Ok(SystemSnapshot::new().with_load(avg[0] as f32))
    }
}
