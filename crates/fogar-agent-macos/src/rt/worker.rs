//! The thread that owns one open device (OS-free; the device code is the `body`).
//!
//! - `open` waits at most a deadline for the body to report ready. If it gives up, the body checks
//!   [`Ctx::starting`] right before starting the device, so the microphone is never switched on
//!   after `open` reported the failure.
//! - Dropping the [`Worker`] signals stop and joins with a deadline; a thread that does not stop
//!   in time is detached and counted instead of blocking the caller.
//! - Every thread nobody waits for any more (timed-out open, including one hung in `play()`, or a
//!   detached drop) is ORPHANED: it counts as in flight for its direction until its [`Ctx`] is
//!   dropped (the thread really ends). While any is in flight, `open` fails at once with
//!   `DeviceBusy` (counted), so stuck threads and streams stay bounded to one per direction.

use super::dsp::COUNTERS;
use fogar_ports::audio::AudioError;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU32, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

/// Deadline for building and starting a stream.
pub(crate) const OPEN_DEADLINE: Duration = Duration::from_secs(5);
/// Deadline for the device thread to stop when its handle is dropped.
pub(crate) const JOIN_DEADLINE: Duration = Duration::from_secs(1);

/// One per direction (input, output): abandoned builds still inside the OS.
#[derive(Debug)]
pub(crate) struct Direction {
    in_flight: AtomicU32,
}

impl Direction {
    pub(crate) const fn new() -> Self {
        Self {
            in_flight: AtomicU32::new(0),
        }
    }
    #[cfg(test)]
    fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::Acquire)
    }
}

pub(crate) static INPUT: Direction = Direction::new();
pub(crate) static OUTPUT: Direction = Direction::new();

const RUNNING: u8 = 0;
const BUILT: u8 = 1;
const STARTING: u8 = 2;
const ABANDONED: u8 = 3;

/// Ownership of a device thread: someone still waits for it, nobody does, or it ended.
const OWNED: u8 = 0;
const ORPHANED: u8 = 1;
const FINISHED: u8 = 2;

/// Owner side: give up on the thread. Counted BEFORE the swap, so the thread can never
/// decrement first; undone at once if the thread already finished.
fn orphan(owner: &AtomicU8, dir: &Direction) {
    dir.in_flight.fetch_add(1, Ordering::AcqRel);
    if owner
        .compare_exchange(OWNED, ORPHANED, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        dir.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What the device thread gets: the phase protocol, the ready channel and the stop signal.
pub(crate) struct Ctx<R> {
    phase: Arc<AtomicU8>,
    owner: Arc<AtomicU8>,
    dir: &'static Direction,
    ready: mpsc::SyncSender<Result<R, AudioError>>,
    /// Disconnected (or a message) = the handle was dropped: stop.
    pub(crate) stop: mpsc::Receiver<()>,
    /// Dropped when the thread ends: tells `Worker::drop` it can join without blocking.
    _done: mpsc::SyncSender<()>,
}

/// The thread really ends here (the body returned or consumed its `Ctx`): if it was orphaned,
/// it stops counting as in flight.
impl<R> Drop for Ctx<R> {
    fn drop(&mut self) {
        if self.owner.swap(FINISHED, Ordering::AcqRel) == ORPHANED {
            self.dir.in_flight.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl<R> Ctx<R> {
    /// `false`: `open` gave up while the build was inside the OS.
    fn leave_running(&self, to: u8) -> bool {
        match self
            .phase
            .compare_exchange(RUNNING, to, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => true,
            Err(seen) => {
                debug_assert_eq!(seen, ABANDONED, "only `open` moves the phase off RUNNING");
                false
            }
        }
    }

    /// The build failed: report it (if anyone still waits). Always call on the error path.
    pub(crate) fn fail(self, e: AudioError) {
        if self.leave_running(BUILT) {
            let _ = self.ready.send(Err(e));
        }
    }

    /// The stream is built, not started. `false`: `open` already gave up, drop the stream and
    /// return WITHOUT starting it.
    pub(crate) fn built(&self) -> bool {
        self.leave_running(BUILT)
    }

    /// Right before starting the device. `false`: abandoned meanwhile, do not start.
    pub(crate) fn starting(&self) -> bool {
        self.phase
            .compare_exchange(BUILT, STARTING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Starting the device failed (after [`Ctx::starting`]): report it to `open`.
    pub(crate) fn start_failed(self, e: AudioError) {
        let _ = self.ready.send(Err(e));
    }

    /// Device started: hand `r` to `open`. `false`: nobody waits any more, stop now.
    pub(crate) fn ready(&self, r: R) -> bool {
        self.ready.send(Ok(r)).is_ok()
    }
}

/// Owns the device thread; dropping it stops the device (joined within [`JOIN_DEADLINE`]).
#[derive(Debug)]
pub(crate) struct Worker {
    owner: Arc<AtomicU8>,
    dir: &'static Direction,
    stop: Option<mpsc::SyncSender<()>>,
    done: mpsc::Receiver<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Worker {
    fn shut(&mut self, deadline: Duration) {
        drop(self.stop.take());
        let Some(t) = self.thread.take() else { return };
        match self.done.recv_timeout(deadline) {
            Err(mpsc::RecvTimeoutError::Timeout) => {
                COUNTERS.threads_detached.fetch_add(1, Ordering::Relaxed);
                // Detached: it ends on its own once the OS call returns, and keeps the
                // direction in flight until then.
                orphan(&self.owner, self.dir);
                drop(t);
            }
            _ => {
                let _ = t.join();
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shut(JOIN_DEADLINE);
    }
}

/// Spawns the device thread running `body` and waits up to `deadline` for it to be ready.
pub(crate) fn spawn<R, F>(
    name: &str,
    dir: &'static Direction,
    deadline: Duration,
    body: F,
) -> Result<(Worker, R), AudioError>
where
    R: Send + 'static,
    F: FnOnce(Ctx<R>) + Send + 'static,
{
    if dir.in_flight.load(Ordering::Acquire) > 0 {
        COUNTERS
            .opens_refused_in_flight
            .fetch_add(1, Ordering::Relaxed);
        return Err(AudioError::DeviceBusy);
    }
    let phase = Arc::new(AtomicU8::new(RUNNING));
    let owner = Arc::new(AtomicU8::new(OWNED));
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (stop_tx, stop_rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let ctx = Ctx {
        phase: phase.clone(),
        owner: owner.clone(),
        dir,
        ready: ready_tx,
        stop: stop_rx,
        _done: done_tx,
    };
    let thread = thread::Builder::new()
        .name(name.into())
        .spawn(move || body(ctx))
        .map_err(|e| AudioError::Backend(Box::new(e)))?;
    let mut worker = Worker {
        owner,
        dir,
        stop: Some(stop_tx),
        done: done_rx,
        thread: Some(thread),
    };
    match ready_rx.recv_timeout(deadline) {
        Ok(Ok(r)) => Ok((worker, r)),
        Ok(Err(e)) => Err(e), // `worker` drops: the thread is already ending
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Never start the device from now on (checked in `built`/`starting`); then let the
            // thread go: unless it already ended, it is orphaned and stays in flight until it
            // ends, wherever it is stuck (build, `play()`, or about to report ready).
            phase.swap(ABANDONED, Ordering::AcqRel);
            worker.shut(Duration::ZERO);
            Err(AudioError::Backend(
                "opening the audio device timed out".into(),
            ))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(AudioError::Backend(
            "the audio thread ended unexpectedly".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::AtomicBool,
        time::{Duration as D, Instant},
    };

    const SHORT: D = D::from_millis(100);

    /// The in-flight mark clears only once the orphaned thread has really ended.
    fn wait_in_flight(dir: &Direction, want: u32) {
        let t0 = Instant::now();
        while dir.in_flight() != want {
            assert!(
                t0.elapsed() < D::from_secs(5),
                "in_flight stuck at {}",
                dir.in_flight()
            );
            thread::sleep(D::from_millis(5));
        }
    }

    #[test]
    fn ready_then_drop_joins() {
        static DIR: Direction = Direction::new();
        let stopped = Arc::new(AtomicBool::new(false));
        let s = stopped.clone();
        let (w, v) = spawn("t-ok", &DIR, SHORT, move |ctx: Ctx<u32>| {
            assert!(ctx.built() && ctx.starting());
            assert!(ctx.ready(7));
            let _ = ctx.stop.recv();
            s.store(true, Ordering::SeqCst);
        })
        .unwrap();
        assert_eq!(v, 7);
        drop(w);
        assert!(
            stopped.load(Ordering::SeqCst),
            "drop returned before the thread stopped"
        );
    }

    #[test]
    fn build_error_is_returned() {
        static DIR: Direction = Direction::new();
        let r = spawn("t-err", &DIR, SHORT, |ctx: Ctx<()>| {
            ctx.fail(AudioError::NoDevice)
        });
        assert!(matches!(r, Err(AudioError::NoDevice)));
        assert_eq!(DIR.in_flight(), 0);
    }

    /// The build blocks past the deadline: `open` fails, the next open is refused at once while
    /// the build is still in flight, the device is never started, and once the build returns
    /// opening works again.
    #[test]
    fn abandoned_build_blocks_reopen_and_never_starts_the_device() {
        static DIR: Direction = Direction::new();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let started = Arc::new(AtomicBool::new(false));
        let st = started.clone();
        let (gone_tx, gone_rx) = mpsc::channel::<bool>();
        let r = spawn("t-slow", &DIR, SHORT, move |ctx: Ctx<()>| {
            let _ = release_rx.recv(); // "inside CoreAudio"
            let built = ctx.built();
            if built && ctx.starting() {
                st.store(true, Ordering::SeqCst);
            }
            let _ = gone_tx.send(built);
        });
        assert!(matches!(r, Err(AudioError::Backend(_))));
        assert_eq!(DIR.in_flight(), 1);
        let refused_before = COUNTERS.opens_refused_in_flight.load(Ordering::Relaxed);
        let t0 = Instant::now();
        let again = spawn("t-again", &DIR, SHORT, |ctx: Ctx<()>| {
            let _ = ctx.ready(());
        });
        assert!(matches!(again, Err(AudioError::DeviceBusy)));
        assert!(t0.elapsed() < SHORT, "refused at once");
        assert!(COUNTERS.opens_refused_in_flight.load(Ordering::Relaxed) > refused_before);
        release_tx.send(()).unwrap();
        assert_eq!(gone_rx.recv_timeout(D::from_secs(2)), Ok(false));
        assert!(
            !started.load(Ordering::SeqCst),
            "device started after open failed"
        );
        wait_in_flight(&DIR, 0);
        let ok = spawn("t-after", &DIR, SHORT, |ctx: Ctx<()>| {
            assert!(ctx.built() && ctx.starting());
            let _ = ctx.ready(());
            let _ = ctx.stop.recv();
        });
        assert!(ok.is_ok());
    }

    /// Timed out after the build but before starting: never started, in flight until it ends.
    #[test]
    fn abandoned_between_build_and_start_never_starts() {
        static DIR: Direction = Direction::new();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (res_tx, res_rx) = mpsc::channel::<bool>();
        let r = spawn("t-mid", &DIR, SHORT, move |ctx: Ctx<()>| {
            assert!(ctx.built());
            let _ = release_rx.recv();
            let _ = res_tx.send(ctx.starting());
        });
        assert!(r.is_err());
        assert_eq!(DIR.in_flight(), 1);
        release_tx.send(()).unwrap();
        assert_eq!(res_rx.recv_timeout(D::from_secs(2)), Ok(false));
        wait_in_flight(&DIR, 0);
    }

    /// Hung inside `play()` (phase STARTING) when `open` gives up: stays in flight, so no second
    /// stream can pile up, until it really ends.
    #[test]
    fn a_hang_in_play_keeps_the_direction_in_flight() {
        static DIR: Direction = Direction::new();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let r = spawn("t-play", &DIR, SHORT, move |ctx: Ctx<()>| {
            assert!(ctx.built() && ctx.starting());
            let _ = release_rx.recv(); // "stuck in play()"
            let _ = ctx.ready(());
        });
        assert!(r.is_err());
        assert_eq!(DIR.in_flight(), 1);
        assert!(matches!(
            spawn("t-2", &DIR, SHORT, |ctx: Ctx<()>| {
                let _ = ctx.ready(());
            }),
            Err(AudioError::DeviceBusy)
        ));
        release_tx.send(()).unwrap();
        wait_in_flight(&DIR, 0);
    }

    #[test]
    fn a_thread_that_does_not_stop_is_detached_and_counted() {
        static DIR: Direction = Direction::new();
        let before = COUNTERS.threads_detached.load(Ordering::Relaxed);
        let (w, ()) = spawn("t-stuck", &DIR, SHORT, |ctx: Ctx<()>| {
            assert!(ctx.built() && ctx.starting());
            let _ = ctx.ready(());
            thread::sleep(D::from_millis(3_000)); // ignores stop
        })
        .unwrap();
        let t0 = Instant::now();
        drop(w);
        let took = t0.elapsed();
        // Returns at the 1 s deadline, well before the thread's 3 s (loaded CI runners: margin).
        assert!(
            took >= JOIN_DEADLINE && took < D::from_millis(2_500),
            "{took:?}"
        );
        assert!(COUNTERS.threads_detached.load(Ordering::Relaxed) > before);
        // The detached thread still holds the direction until it ends.
        assert_eq!(DIR.in_flight(), 1);
        assert!(matches!(
            spawn("t-3", &DIR, SHORT, |ctx: Ctx<()>| {
                let _ = ctx.ready(());
            }),
            Err(AudioError::DeviceBusy)
        ));
        wait_in_flight(&DIR, 0);
    }
}
