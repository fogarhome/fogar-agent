//! E7: same program under panic=unwind vs panic=abort.
use std::io::Write;
use std::time::Duration;

fn install_hook() {
    std::panic::set_hook(Box::new(|info| {
        // Synchronous, allocation-light crash record. Do not rely on a non-blocking
        // log writer here: with panic=abort its buffer dies with the process.
        let mut f = std::fs::OpenOptions::new().create(true).append(true)
            .open(std::env::temp_dir().join("e7-crash.log")).unwrap();
        let bt = std::backtrace::Backtrace::force_capture();
        let thread = std::thread::current();
        let _ = writeln!(f, "panic thread={:?} {info}\n{bt}", thread.name());
        let _ = f.sync_all();
        eprintln!("hook: crash record written for: {info}");
    }));
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    install_hook();
    let h = tokio::spawn(async { panic!("bug in session engine") });
    match h.await {
        Err(e) if e.is_panic() => eprintln!("unwind: JoinError::is_panic() == true; process still alive"),
        other => eprintln!("other: {other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    eprintln!("main reached end normally");
}
