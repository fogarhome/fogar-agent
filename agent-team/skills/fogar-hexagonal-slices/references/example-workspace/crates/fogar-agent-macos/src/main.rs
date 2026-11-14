//! COMPOSITION ROOT. The only place that knows concrete adapter types.
//! No DI framework: construct adapters, hand them to the core, run.

fn main() {
    // current_thread runtime: a mostly-idle daemon needs no worker pool (fewer threads,
    // fewer wakeups). Blocking OS calls go through spawn_blocking or adapter-owned threads.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("tokio runtime");
    rt.block_on(run());
}

async fn run() {
    cfg_select! {
        target_os = "macos" => {
            use fogar_core::governor::{Governor, Policy};
            use fogar_host::{StderrUplink, TokioClock};
            let probe = fogar_agent_macos::mac::MacSystemProbe::new().expect("probe");
            let mut gov = Governor::new(probe, StderrUplink, TokioClock::default(), Policy::default());
            tokio::select! {
                _ = gov.run() => {}
                _ = tokio::signal::ctrl_c() => {} // real daemon: also SIGTERM from launchd
            }
        }
        _ => {
            // The crate still builds on every host so Linux CI and every worktree can run
            // `cargo check/clippy --workspace` and the archtest.
            eprintln!("fogar-agent-macos only runs on macOS");
        }
    }
}
