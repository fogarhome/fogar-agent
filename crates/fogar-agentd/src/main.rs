//! fogar-agentd: composition root. Builds the platform adapters (the only `cfg(target_os)` wiring
//! in the workspace besides adapter crates), installs the crash hook and the crash-loop breaker,
//! runs the engine on a dedicated runtime thread, and performs the ordered SIGTERM shutdown.
//!
//! Usage:
//!   fogar-agentd run --gateway ws://127.0.0.1:8004/ws/stream [--agent NAME] [--state-dir DIR]
//!                    [--keepalive-secs N] [--no-ipc]
//!   fogar-agentd store-key [--state-dir DIR]      (reads the gateway client key from stdin)
#![forbid(unsafe_code)] // OS calls go through adapters or rustix's safe wrappers (review sec#7)

mod cli;
mod crash;
mod ledger;
mod state_dir;
mod status_log;

std::cfg_select! {
    target_os = "linux" => {
        mod platform_linux;
        use platform_linux as platform;
    }
    target_os = "macos" => {
        mod platform_macos;
        use platform_macos as platform;
    }
    _ => {
        compile_error!("fogar-agentd has no platform adapters for this OS yet (roadmap S8)");
    }
}

use fogar_core::{Agent, AgentPorts, LinkState, crashloop};
use fogar_host::{PrngRandom, TokioClock, WsConfig, WsGatewayLink};
use fogar_ports::{
    power::WorkClass,
    secrets::{SecretKey, SecretString},
};
use std::{
    io::Read,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Below launchd's ExitTimeOut (fogar-longrun-reliability: aim < 5 s).
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(4);
/// Uptime after which this boot counts as stable for the crash-loop breaker.
const STABLE_AFTER: Duration = Duration::from_secs(600);
/// At most `STATUS_LOG_BURST` link status lines per `STATUS_LOG_WINDOW` on stderr.
const STATUS_LOG_WINDOW: Duration = Duration::from_secs(60);
const STATUS_LOG_BURST: u32 = 20;
/// The OS-thread watchdog aborts if the runtime heartbeat is older than this.
const WATCHDOG_STALE: Duration = Duration::from_secs(60);

fn main() -> ExitCode {
    // Before anything holds the client key (review sec#7).
    if let Err(e) = state_dir::disable_core_dumps() {
        eprintln!("fogar-agentd: could not disable core dumps: {e}");
    }
    let cmd = match cli::parse(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("fogar-agentd: {msg}\n{}", cli::USAGE);
            return ExitCode::from(64);
        }
    };
    match cmd {
        cli::Command::StoreKey { state_dir } => match private_state_dir(state_dir) {
            Ok(dir) => store_key(&dir),
            Err(code) => code,
        },
        cli::Command::Run(opts) => run(opts),
    }
}

/// Resolves and validates the state directory before anything touches it (review sec#1). A bad
/// state dir is a configuration error (EX_CONFIG), reported before the boot ledger exists, so it
/// is never counted as a crash.
fn private_state_dir(explicit: Option<std::path::PathBuf>) -> Result<std::path::PathBuf, ExitCode> {
    let Some(dir) = explicit.or_else(platform::default_state_dir) else {
        eprintln!(
            "fogar-agentd: no state directory: HOME (or XDG_STATE_HOME) is unset or relative; \
             pass --state-dir DIR (there is no shared /tmp fallback)"
        );
        return Err(ExitCode::from(78));
    };
    if let Err(e) = state_dir::ensure_private_dir(&dir) {
        eprintln!(
            "fogar-agentd: state directory {} is not a private directory owned by this user: {e}",
            dir.display()
        );
        return Err(ExitCode::from(78));
    }
    Ok(dir)
}

fn store_key(state_dir: &std::path::Path) -> ExitCode {
    // Zeroized on drop (review sec#7, SEC-2); the SecretString below zeroizes its own copy.
    let mut key = zeroize::Zeroizing::new(String::new());
    if std::io::stdin().read_to_string(&mut key).is_err() || key.trim().is_empty() {
        eprintln!("fogar-agentd: expected the client key on stdin");
        return ExitCode::from(64);
    }
    let store = match platform::secret_store(state_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fogar-agentd: secret store unavailable: {e}");
            return ExitCode::from(74);
        }
    };
    match store.put(
        SecretKey::GatewayClientKey,
        SecretString::from(key.trim().to_owned()),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fogar-agentd: could not store the key: {e}");
            ExitCode::from(74)
        }
    }
}

fn run(opts: cli::RunOpts) -> ExitCode {
    // Validate configuration before the boot ledger records a start: a usage error is not a crash.
    let ws = match WsConfig::new(opts.gateway.clone()) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("fogar-agentd: {e}");
            return ExitCode::from(64);
        }
    };
    // Review sec#3: slice 0 has no TLS; WsConfig accepts only loopback ws:// (wss:// is refused
    // until S1). Allowed, but never silent.
    eprintln!(
        "fogar-agentd: WARNING: the gateway link is UNENCRYPTED ws:// (loopback only). Local \
         processes that can reach or proxy that port see the client key in the handshake. TLS \
         (wss://) arrives with S1 (ADR-0006 item 4)."
    );
    let state_dir = match private_state_dir(opts.state_dir.clone()) {
        Ok(d) => d,
        Err(code) => return code,
    };
    crash::install_hook(&state_dir);
    let ledger = ledger::Ledger::new(&state_dir);

    // One runtime thread (current_thread: lowest idle cost, fogar-energy-budget). The main thread
    // only waits; on macOS it becomes the CFRunLoop thread when Apple frameworks need it (S2/S4).
    let rt_ledger = ledger.clone();
    let rt_thread = std::thread::Builder::new()
        .name("fogar-rt".into())
        .spawn(move || {
            let power = platform::power_policy();
            let _ = power.apply_process_baseline();
            let _ = power.apply_current_thread(WorkClass::Control);
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("fogar-agentd: runtime: {e}");
                    return None;
                }
            };
            let stopped = rt.block_on(async move {
                // Review code#7b: the SIGTERM/SIGINT handlers are installed BEFORE this boot is
                // recorded as `start` (and before the platform adapters and the engine are
                // built), so any stop after the `start` line is an ordinary stop, not a crash.
                // A signal before this point kills the process before `start` is written, which
                // the breaker does not count either.
                let stop = StopSignal::install();
                let boot = rt_ledger.begin_boot();
                eprintln!("fogar-agentd: boot mode {}", boot.label());
                serve(opts, state_dir, ws, boot, stop).await
            });
            // A stuck blocking task must not hold the exit (fogar-longrun shutdown step 4).
            rt.shutdown_timeout(Duration::from_secs(1));
            Some(stopped)
        });
    let stopped = match rt_thread {
        Ok(h) => h.join().ok().flatten(),
        Err(e) => {
            eprintln!("fogar-agentd: cannot start runtime thread: {e}");
            None
        }
    };
    if let Some(stopped) = stopped {
        // Review code#2: a stop asked for by a signal is an ordinary stop for the crash-loop
        // breaker even if a shutdown phase overran its deadline; the overrun is reported here
        // (and in the shutdown report line) instead of being counted as a crash.
        if let Some(r) = stopped.report.filter(|r| !r.clean) {
            eprintln!("fogar-agentd: shutdown overran its budget (not a crash): {r:?}");
        }
        ledger.mark_clean();
    }
    // Exit 0 even after an unclean stop: a non-zero exit after SIGTERM would make launchd/systemd
    // restart a process that was asked to stop. The ledger keeps the truth.
    ExitCode::SUCCESS
}

/// How `serve` ended. It only ever returns after a stop signal; `report` is `None` when the
/// engine never started (SafeMode delay interrupted, adapters unavailable).
#[derive(Debug)]
struct Stopped {
    report: Option<fogar_core::ShutdownReport>,
}

async fn serve(
    opts: cli::RunOpts,
    state_dir: std::path::PathBuf,
    ws: WsConfig,
    boot: crashloop::BootMode,
    mut stop: StopSignal,
) -> Stopped {
    let stopped = Stopped { report: None };
    if let crashloop::BootMode::SafeMode {
        delay,
        recent_crashes,
    } = boot
    {
        eprintln!(
            "fogar-agentd: {recent_crashes} recent crashes; safe mode, starting in {delay:?}"
        );
        tokio::select! {
            () = stop.recv() => return stopped,
            () = tokio::time::sleep(delay) => {}
        }
    }
    let p = match platform::build(&state_dir, !opts.no_ipc) {
        Ok(p) => p,
        Err(e) => {
            // Stay alive degraded rather than exit (launchd would respawn in a loop).
            eprintln!("fogar-agentd: platform adapters unavailable: {e}; waiting for SIGTERM");
            stop.recv().await;
            return stopped;
        }
    };
    let clock = TokioClock::new();
    let mut cfg = fogar_core::Config {
        agent: opts.agent.clone(),
        ..fogar_core::Config::default()
    };
    if let Some(k) = opts.keepalive_secs {
        cfg.keepalive_after = (k > 0).then(|| Duration::from_secs(k));
    }
    let ports = AgentPorts {
        clock: Arc::new(clock),
        link: WsGatewayLink::new(ws),
        ipc: p.ipc,
        secrets: p.secrets,
        random: Arc::new(PrngRandom::new()),
        audio_in: p.audio_in,
        audio_out: p.audio_out,
        probe: Some(p.probe),
    };
    let agent = Agent::start(ports, cfg, boot);
    let watchdog = Watchdog::spawn(agent.heartbeat(), clock.epoch_std(), state_dir.clone());
    let mut health = agent.health();
    let mut last = String::new();
    let mut limiter = status_log::LogLimiter::new(STATUS_LOG_WINDOW, STATUS_LOG_BURST);
    let stable = tokio::time::sleep(STABLE_AFTER);
    tokio::pin!(stable);
    let mut stable_done = false;
    loop {
        tokio::select! {
            biased;
            () = stop.recv() => break,
            v = health.wait_link(|v| v.state.label() != last) => {
                last = v.state.label().to_owned();
                match limiter.check(std::time::Instant::now()) {
                    status_log::Verdict::Suppress => continue,
                    status_log::Verdict::Print(Some(n)) => {
                        eprintln!("fogar-agentd: ({n} link status lines suppressed)");
                    }
                    status_log::Verdict::Print(None) => {}
                }
                match v.last_close {
                    Some((code, action)) if matches!(v.state, LinkState::Backoff { .. } | LinkState::Parked { .. }) => {
                        eprintln!("fogar-agentd: link {last} (last close {code} -> {action:?})");
                    }
                    _ => eprintln!("fogar-agentd: link {last}"),
                }
            }
            () = &mut stable, if !stable_done => {
                stable_done = true;
                ledger::Ledger::new(&state_dir).mark_stable();
            }
        }
    }
    eprintln!("fogar-agentd: shutting down");
    let report = agent.shutdown(SHUTDOWN_BUDGET).await;
    watchdog.stop();
    eprintln!("fogar-agentd: shutdown {report:?}");
    Stopped {
        report: Some(report),
    }
}

/// SIGTERM (launchd/systemd stop) or SIGINT (dev), registered eagerly by `install` (a lazily
/// polled `signal()` future would leave the default "terminate" action in place until the first
/// poll; review code#7b).
struct StopSignal {
    #[cfg(unix)]
    streams: Option<(tokio::signal::unix::Signal, tokio::signal::unix::Signal)>,
}

impl StopSignal {
    /// Must run inside the runtime.
    fn install() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            match (
                signal(SignalKind::terminate()),
                signal(SignalKind::interrupt()),
            ) {
                (Ok(t), Ok(i)) => Self {
                    streams: Some((t, i)),
                },
                (Err(e), _) | (_, Err(e)) => {
                    eprintln!("fogar-agentd: cannot install signal handlers: {e}");
                    Self { streams: None }
                }
            }
        }
        #[cfg(not(unix))]
        {
            Self {}
        }
    }

    /// Cancel-safe: resolves on the next SIGTERM/SIGINT.
    async fn recv(&mut self) {
        #[cfg(unix)]
        {
            match self.streams.as_mut() {
                Some((term, int)) => {
                    tokio::select! {
                        _ = term.recv() => {}
                        _ = int.recv() => {}
                    }
                }
                None => std::future::pending().await,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

/// OS-thread watchdog over the runtime heartbeat (fogar-longrun "Watchdog"): if the runtime is
/// starved or deadlocked the heartbeat goes stale and this thread, outside tokio, aborts the
/// process after writing a crash record; the supervisor restarts it.
struct Watchdog {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    fn spawn(
        hb: Arc<fogar_core::heartbeat::Heartbeat>,
        epoch: std::time::Instant,
        state_dir: std::path::PathBuf,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::Builder::new()
            .name("fogar-watchdog".into())
            .spawn(move || {
                while !flag.load(Ordering::Acquire) {
                    std::thread::park_timeout(Duration::from_secs(10));
                    let now_ms = u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
                    if hb.age(now_ms)
                        > u64::try_from(WATCHDOG_STALE.as_millis()).unwrap_or(u64::MAX)
                        && !flag.load(Ordering::Acquire)
                    {
                        crash::write_record(
                            &state_dir,
                            "watchdog: runtime heartbeat stale; aborting",
                        );
                        std::process::abort();
                    }
                }
            })
            .ok();
        Self { stop, handle }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            h.thread().unpark();
            let _ = h.join();
        }
    }
}
