//! E8: compile-checked building blocks used in the skill's references.
use arc_swap::ArcSwap;
use secrecy::{ExposeSecret, SecretString};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tracing_subscriber::{fmt, prelude::*, reload, EnvFilter};

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// ---------- 1. Logging: non-blocking, lossy, bounded, rotated, count-capped ----------
fn init_logging(dir: &std::path::Path) -> (tracing_appender::non_blocking::WorkerGuard,
    reload::Handle<EnvFilter, tracing_subscriber::Registry>, tracing_appender::non_blocking::ErrorCounter) {
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY) // time-based only (no size cap)
        .filename_prefix("fogar-agent")
        .filename_suffix("log")
        .max_log_files(7)
        .build(dir)
        .expect("log dir");
    let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .buffered_lines_limit(8_192) // default 128_000 lines; keep the bound small
        .lossy(true)                 // drop, never block the runtime
        .finish(appender);
    let dropped = writer.error_counter();
    let (filter, handle) = reload::Layer::new(EnvFilter::new("info,fogar=info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(writer).with_ansi(false))
        .init();
    (guard, handle, dropped)
}

// ---------- 2. Backpressure policies ----------
/// Latest-value-wins (state sync): never queues, never blocks.
fn coalescing_state() -> (watch::Sender<u64>, watch::Receiver<u64>) { watch::channel(0) }

/// Drop-oldest bounded buffer for telemetry samples (owner-local, no lock).
struct DropOldest<T> { buf: VecDeque<T>, cap: usize, dropped: u64 }
impl<T> DropOldest<T> {
    fn push(&mut self, v: T) {
        if self.buf.len() == self.cap { self.buf.pop_front(); self.dropped += 1; }
        self.buf.push_back(v);
    }
}

/// Shed-newest with accounting (try_send).
fn offer<T>(tx: &mpsc::Sender<T>, v: T, shed: &mut u64) {
    match tx.try_send(v) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => *shed += 1,
        Err(mpsc::error::TrySendError::Closed(_)) => {} // owner gone: shutting down
    }
}

// ---------- 3. Wall-clock / sleep jump detector ----------
/// Instant on macOS = CLOCK_UPTIME_RAW (does not advance while asleep).
/// Comparing it with SystemTime reveals suspend (and manual clock changes).
struct JumpDetector { mono: Instant, wall: SystemTime }
impl JumpDetector {
    fn new() -> Self { Self { mono: Instant::now(), wall: SystemTime::now() } }
    /// Returns Some(skew) if wall time advanced more than monotonic time by > tolerance.
    fn check(&mut self, tolerance: Duration) -> Option<i128> {
        let m = self.mono.elapsed().as_millis() as i128;
        let w = match SystemTime::now().duration_since(self.wall) {
            Ok(d) => d.as_millis() as i128,
            Err(e) => -(e.duration().as_millis() as i128), // wall clock went backwards
        };
        self.mono = Instant::now();
        self.wall = SystemTime::now();
        let skew = w - m;
        (skew.abs() > tolerance.as_millis() as i128).then_some(skew)
    }
}

// ---------- 4. Hot config reload ----------
#[derive(Debug)]
struct Config { heartbeat: Duration, log_filter: String }

// ---------- 5. Process probes ----------
fn heap_stats() -> (usize, usize) {
    tikv_jemalloc_ctl::epoch::advance().ok();
    let allocated = tikv_jemalloc_ctl::stats::allocated::read().unwrap_or(0);
    let resident = tikv_jemalloc_ctl::stats::resident::read().unwrap_or(0);
    (allocated, resident)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let dir = std::env::temp_dir().join("e8-logs");
    std::fs::create_dir_all(&dir).unwrap();
    let (_guard, filter_handle, dropped) = init_logging(&dir);
    tracing::info!(target: "fogar", "started");

    // config reload: readers take a cheap snapshot; writer swaps atomically
    let cfg = Arc::new(ArcSwap::from_pointee(Config { heartbeat: Duration::from_secs(20), log_filter: "info".into() }));
    let new = Config { heartbeat: Duration::from_secs(15), log_filter: "debug".into() };
    filter_handle.reload(EnvFilter::new(&new.log_filter)).unwrap();
    cfg.store(Arc::new(new));
    let snap = cfg.load();
    tracing::debug!(target: "fogar", hb = ?snap.heartbeat, "config reloaded");

    let (st_tx, mut st_rx) = coalescing_state();
    for v in 1..=1000 { st_tx.send_replace(v); }
    st_rx.changed().await.unwrap();
    println!("watch coalesced 1000 updates -> receiver sees {}", *st_rx.borrow_and_update());

    let mut d = DropOldest { buf: VecDeque::with_capacity(4), cap: 4, dropped: 0 };
    for i in 0..10 { d.push(i); }
    println!("drop-oldest kept {:?}, dropped {}", d.buf, d.dropped);

    let (tx, _rx) = mpsc::channel::<u32>(2);
    let mut shed = 0;
    for i in 0..5 { offer(&tx, i, &mut shed); }
    println!("shed-newest: shed {shed} of 5");

    let mut jd = JumpDetector::new();
    tokio::time::sleep(Duration::from_millis(50)).await;
    println!("jump detector (no suspend) -> {:?}", jd.check(Duration::from_secs(2)));

    let token = SecretString::from("s3cr3t-token".to_string());
    println!("secret Debug prints: {:?}; length {}", token, token.expose_secret().len());

    let (a, r) = heap_stats();
    println!("jemalloc allocated={a} resident={r}; log lines dropped={}", dropped.dropped_lines());
}
