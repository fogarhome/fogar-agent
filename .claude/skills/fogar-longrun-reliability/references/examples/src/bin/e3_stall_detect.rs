//! E3: (a) detect a blocked executor with an in-runtime heartbeat + out-of-runtime
//! OS-thread watchdog; (b) blocking-pool saturation -> queueing latency;
//! (c) a panicking task whose JoinHandle is dropped is invisible without a hook.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
fn now_ms() -> u64 { START.get().unwrap().elapsed().as_millis() as u64 }

fn main() {
    START.set(Instant::now()).unwrap();
    // Panic hook: always log (in prod: also write a crash record synchronously).
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("[{:>5} ms] PANIC HOOK: {info}", now_ms());
        let _ = &default; // in prod you may chain to default for backtrace
    }));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(2) // deliberately tiny to show saturation
        .enable_all()
        .build()
        .unwrap();

    // Heartbeat written by a task on the runtime; read by an OS thread.
    let beat = Arc::new(AtomicU64::new(now_ms()));
    {
        let beat = beat.clone();
        rt.spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_millis(50));
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut last = Instant::now();
            loop {
                iv.tick().await;
                let lag = last.elapsed().saturating_sub(Duration::from_millis(50));
                if lag > Duration::from_millis(100) {
                    println!("[{:>5} ms] in-runtime probe: tick lag {:?} (scheduler starved)", now_ms(), lag);
                }
                last = Instant::now();
                beat.store(now_ms(), Ordering::Relaxed);
            }
        });
    }
    {
        let beat = beat.clone();
        std::thread::Builder::new().name("watchdog".into()).spawn(move || loop {
            std::thread::sleep(Duration::from_millis(100));
            let age = now_ms().saturating_sub(beat.load(Ordering::Relaxed));
            if age > 400 {
                println!("[{:>5} ms] OS watchdog: runtime heartbeat stale for {age} ms", now_ms());
            }
        }).unwrap();
    }

    rt.block_on(async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        // (a) Block BOTH workers with synchronous sleeps (the classic bug).
        println!("[{:>5} ms] blocking both workers for 1s with std::thread::sleep", now_ms());
        for _ in 0..2 {
            tokio::spawn(async { std::thread::sleep(Duration::from_secs(1)); });
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // (b) Blocking pool saturation: 2 threads, 3 jobs of 500ms each.
        let t = Instant::now();
        let mut hs = vec![];
        for i in 0..3 {
            hs.push(tokio::task::spawn_blocking(move || {
                let queued = t.elapsed();
                std::thread::sleep(Duration::from_millis(500));
                (i, queued)
            }));
        }
        for h in hs {
            let (i, q) = h.await.unwrap();
            println!("[{:>5} ms] blocking job {i} started after {:?} in queue", now_ms(), q);
        }
        let m = tokio::runtime::Handle::current().metrics();
        println!("[{:>5} ms] stable metrics: workers={} alive_tasks={} global_queue_depth={}",
            now_ms(), m.num_workers(), m.num_alive_tasks(), m.global_queue_depth());

        // (c) Panic in a detached task.
        let _ = tokio::spawn(async { panic!("invariant violated in detached task") });
        tokio::time::sleep(Duration::from_millis(100)).await;
        println!("[{:>5} ms] main continues; without the hook this panic would only hit stderr", now_ms());
    });
}
