//! E2: ordered graceful shutdown with CancellationToken (hierarchy) + TaskTracker,
//! a deadline, a blocking task that ignores cancellation, and Runtime::shutdown_timeout.
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

fn log(t0: Instant, s: &str) {
    println!("[{:>5} ms] {s}", t0.elapsed().as_millis());
}

fn main() {
    let t0 = Instant::now();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async move {
        let root = CancellationToken::new();
        // Phase tokens: producers stop first, then the writer drains, then I/O closes.
        let ingress = root.child_token();
        let egress = CancellationToken::new(); // cancelled explicitly AFTER ingress drained

        let data_plane = TaskTracker::new(); // phase 1: producers + writer
        let io_plane = TaskTracker::new();   // phase 2: connections
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(64);

        // Producer (ingress)
        {
            let tok = ingress.clone();
            data_plane.spawn(async move {
                let mut i = 0;
                loop {
                    tokio::select! {
                        biased;
                        _ = tok.cancelled() => break,
                        p = tx.reserve() => { match p { Ok(p) => { p.send(i); i += 1; } Err(_) => break } }
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                // dropping tx closes the channel -> writer drains then exits
                log(t0, &format!("producer stopped after {i} msgs"));
            });
        }
        // Writer (drains channel fully; no token: ends when all senders dropped)
        data_plane.spawn(async move {
            let mut n = 0;
            while let Some(_m) = rx.recv().await { n += 1; }
            tokio::time::sleep(Duration::from_millis(50)).await; // simulated final flush/COMMIT
            log(t0, &format!("writer drained {n} msgs and flushed"));
        });
        // Connection (egress): sends a close frame on cancel
        {
            let tok = egress.clone();
            io_plane.spawn(async move {
                tok.cancelled().await;
                tokio::time::sleep(Duration::from_millis(20)).await; // simulated WS close handshake
                log(t0, "connection closed cleanly");
            });
        }
        // A blocking task that does NOT observe cancellation (e.g. stuck FFI call)
        let _stuck = tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_secs(30));
        });

        data_plane.close();
        io_plane.close();
        tokio::time::sleep(Duration::from_millis(200)).await;
        log(t0, "SIGTERM received -> phase 1: cancel ingress, drain writer");
        ingress.cancel();
        if tokio::time::timeout(Duration::from_secs(2), data_plane.wait()).await.is_err() {
            log(t0, "phase 1 DEADLINE hit");
        }
        log(t0, "phase 2: cancel egress (close sockets)");
        egress.cancel();
        if tokio::time::timeout(Duration::from_secs(1), io_plane.wait()).await.is_err() {
            log(t0, "phase 2 DEADLINE hit");
        }
        root.cancel();
        log(t0, "all phases done");
    });
    log(t0, "block_on returned; runtime drop would wait ~30s for the stuck blocking task");
    rt.shutdown_timeout(Duration::from_millis(500));
    log(t0, "shutdown_timeout(500ms) returned; process exits (blocking thread leaked)");
}
