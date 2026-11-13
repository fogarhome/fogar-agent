//! UNTESTED. Load average only; everything else honestly `Unsupported` until its slice (S5).
//!
//! TODO(S5, fogar-system-probe §2): memory pressure via `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE`
//! (dispatch2 0.3.1) seeded from sysctl `kern.memorystatus_vm_pressure_level`; thermal via
//! notify(3) `kOSThermalNotificationPressureLevelName` (`notify_register_file_descriptor`) or
//! `NSProcessInfo.thermalState`; AC/battery via `IOPSCopyPowerSourcesInfo` + notify
//! `com.apple.system.powersources.source`; Low Power Mode via
//! `NSProcessInfo.isLowPowerModeEnabled` + `NSProcessInfoPowerStateDidChangeNotification`; user
//! idle via IORegistry `IOHIDSystem` -> `HIDIdleTime`; CPU via `host_processor_info` (must
//! `vm_deallocate` the result). `host_statistics64` is rate-limited for 3rd-party binaries.

use fogar_ports::{
    Support,
    system_probe::{ProbeCaps, ProbeError, SystemProbe, SystemSnapshot},
};

unsafe extern "C" {
    /// libSystem `getloadavg(3)`.
    fn getloadavg(loadavg: *mut f64, nelem: i32) -> i32;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MacSystemProbe;

impl SystemProbe for MacSystemProbe {
    fn capabilities(&self) -> ProbeCaps {
        let mut c = ProbeCaps::NONE;
        c.load = Support::Native;
        c
    }

    fn snapshot(&self) -> Result<SystemSnapshot, ProbeError> {
        let mut avg = [0f64; 3];
        // SAFETY: `avg` is a valid, aligned, writable buffer of 3 doubles and we pass nelem = 3;
        // getloadavg writes at most nelem values and does not retain the pointer.
        let n = unsafe { getloadavg(avg.as_mut_ptr(), 3) };
        if n < 1 {
            return Err(ProbeError::Platform(Box::new(
                std::io::Error::last_os_error(),
            )));
        }
        Ok(SystemSnapshot::new().with_load(avg[0].max(0.0) as f32))
    }
}
