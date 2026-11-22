//! OS-free parts of the audio adapter, compiled and tested on every host (`cfg(test)` on Linux):
//! the real-time ring writer/reader, counters and resampler (`dsp`) and the capture loop
//! (`pump`) and the device-thread lifecycle (`worker`). The macOS module `audio` wires them to cpal and AVFoundation.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

pub(crate) mod dsp;
pub(crate) mod pump;
pub(crate) mod worker;

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    #[global_allocator]
    static A: fogar_testalloc::Counting = fogar_testalloc::Counting;

    static SERIAL: Mutex<()> = Mutex::new(());

    /// Fails if `f` grows the heap at all (peak above the starting live size). Every attempt
    /// runs `f` ONCE on a fresh state from `setup` (built outside the measurement), so an
    /// allocation on the first call shows up in every attempt and retries cannot hide it. The
    /// allocator counts process-wide and tests run in parallel: a measurement disturbed by
    /// another thread is retried.
    pub(crate) fn assert_no_alloc<S>(
        what: &str,
        mut setup: impl FnMut() -> S,
        mut f: impl FnMut(&mut S),
    ) {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let mut last = 0;
        for _ in 0..100 {
            let mut state = setup();
            fogar_testalloc::reset_peak();
            let before = fogar_testalloc::live_bytes();
            f(&mut state);
            last = fogar_testalloc::peak_bytes() - before;
            let clean = last <= 0 && fogar_testalloc::live_bytes() == before;
            drop(state);
            if clean {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("{what} allocated ({last} bytes above the starting heap, 100 attempts)");
    }
}
