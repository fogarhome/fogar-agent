//! UNTESTED. Thread QoS via `pthread_set_qos_class_self_np` (fogar-energy-budget §QoS map):
//! Control -> QOS_CLASS_UTILITY, Bulk -> QOS_CLASS_BACKGROUND. Never `setpriority` /
//! `pthread_setschedparam` on QoS threads (opts the thread out of QoS). The socket-owning thread
//! must never be BACKGROUND (darwinbg => background sockets).
//!
//! Process baseline is packaging, not code: launchd `ProcessType` / `LowPriorityIO` in the
//! LaunchAgent plist (S7), so it reports Unsupported here.
//! TODO(S7, measure on the M2): `ProcessType=Background` vs `Standard` for wake-word latency.

use fogar_ports::{
    Support,
    power::{Applied, PowerCaps, PowerError, PowerPolicy, WorkClass},
};

/// `<sys/qos.h>` values (qos_class_t is an unsigned int enum).
const QOS_CLASS_UTILITY: u32 = 0x11;
const QOS_CLASS_BACKGROUND: u32 = 0x09;

unsafe extern "C" {
    /// `<pthread/qos.h>`.
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MacPowerPolicy;

impl PowerPolicy for MacPowerPolicy {
    fn capabilities(&self) -> PowerCaps {
        let mut c = PowerCaps::NONE;
        c.thread_class = Support::Native;
        c
    }

    fn apply_process_baseline(&self) -> Result<Applied, PowerError> {
        Ok(Applied::NotSupported)
    }

    fn apply_current_thread(&self, class: WorkClass) -> Result<Applied, PowerError> {
        let qos = match class {
            WorkClass::Control => QOS_CLASS_UTILITY,
            WorkClass::Bulk => QOS_CLASS_BACKGROUND,
            _ => QOS_CLASS_UTILITY,
        };
        // SAFETY: plain value arguments; affects only the calling thread; relative priority 0 is
        // within the documented range [QOS_MIN_RELATIVE_PRIORITY, 0].
        let rc = unsafe { pthread_set_qos_class_self_np(qos, 0) };
        if rc == 0 {
            Ok(Applied::Applied)
        } else {
            Err(PowerError::Platform(Box::new(std::io::Error::from_raw_os_error(rc))))
        }
    }
}
