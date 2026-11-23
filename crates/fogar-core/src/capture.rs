//! The single consent point of capture (spec S2, «Requisito duro: punto único de consentimiento
//! de captura»; sec#8 of the slice-0 review).
//!
//! **Structure, not discipline.**
//! - [`CaptureGate`] is the only owner of the microphone port: `AgentPorts` hands it the
//!   `AudioSource` and the engine keeps only the gate (archtest R11: no other file names the
//!   port; R13: no other file asks for the OS permission).
//! - Opening a take needs a [`Start`]. The only one in S2 is [`Start::UserAct`], whose
//!   [`UserAct`] only `ipc.rs` can build, from a `CaptureStart` of a same-signer connection
//!   subscribed to the conversation. No gateway message, reconnect, boot or timer has a way to
//!   make one.
//! - A take opens only when ALL hold at that moment ([`decide`]): a valid `Start`; capture
//!   supported and not backing off a failed open; permission `Granted` (`mic_permission()`,
//!   which never prompts; `Unknown` only on a platform without a permission model); link ready;
//!   transcriber alive; no other take and no device open/release in flight. Otherwise a
//!   [`CaptureRefusal`] says why.
//! - A take always closes on: the person's `CaptureEnd`; its connection closing or leaving the
//!   conversation (owner channel dropped => `Discard`); the silence cut and the safety cut
//!   (`Review`, never sent: «un corte automático nunca envía»); the device stream ending
//!   (`Review`); the link going down; the agent stopping. Closing = releasing the
//!   `ActiveCapture`.
//!
//! **Off the runtime (issue #24).** `AudioSource::open` and dropping an `ActiveCapture` may block
//! (`OPEN_BLOCK_MAX` / `DROP_BLOCK_MAX`). The gate runs both on the blocking pool through its own
//! `JoinSet` (handles moved in), at most [`queues::CAPTURE_JOBS`] at a time; the engine task, the
//! WebSocket, the heartbeat and the signals keep running meanwhile. An open is bounded by
//! [`OPEN_DEADLINE`] on the core side too; a late result is released, never used. Failed opens back
//! off (`OPEN_BACKOFF_BASE` doubling up to `OPEN_BACKOFF_CAP`) before the device is touched again.
//!
//! **Logging without content.** The core has no logger: every open, refusal (by reason) and cut
//! is counted in `Stats`; nothing about the audio or the transcript is kept here.

use crate::{config::Config, ipc::UserAct, queues, stats::Stats, time::sleep_until_opt};
use fogar_ports::{
    Support,
    audio::{
        ActiveCapture, AudioChunk, AudioError, AudioFormat, AudioSource, MicPermission,
        OPEN_BLOCK_MAX,
    },
    clock::{Clock, Mono},
    events::EventStream,
    ipc::{CaptureGesture, CaptureRefusal, CaptureThen, Notice, ServiceState},
};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::{
    sync::oneshot,
    task::{AbortHandle, JoinSet},
};

/// **Issue #31.** Running the device calls on tokio's blocking pool is a temporary exception
/// (review M2, see [`spawn_device_job`]) until the audio port is async in the adapter.
///
/// Core-side bound on one open (the adapter's own bound plus slack): past it the take is
/// refused and the open's late result, if any, is released.
pub(crate) const OPEN_DEADLINE: Duration =
    OPEN_BLOCK_MAX.saturating_add(Duration::from_millis(500));
/// How long shutdown waits for the device release before it leaves it to the blocking pool
/// (the adapter bounds the drop itself by `DROP_BLOCK_MAX`).
pub(crate) const RELEASE_WAIT_ON_STOP: Duration = Duration::from_millis(500);
const OPEN_BACKOFF_BASE: Duration = Duration::from_millis(500);
const OPEN_BACKOFF_CAP: Duration = Duration::from_secs(8);
/// Concurrent `RequestMicPermission` waiters on one pending prompt (one per IPC connection).
const PERMISSION_WAITERS: usize = queues::IPC_MAX_CONNS;

/// What may open a take. S2 has exactly one: the person's explicit act (S5 adds the wake word,
/// with its own condition).
#[derive(Debug)]
pub(crate) enum Start {
    UserAct(UserAct),
}

impl Start {
    fn gesture(&self) -> CaptureGesture {
        match self {
            Start::UserAct(a) => a.gesture(),
        }
    }
}

/// What the gate needs from outside, read when the request arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Readiness {
    /// A gateway session is `ready`.
    pub(crate) link_ready: bool,
    /// The transcriber as last heard (`ready.live_capabilities` + `status`).
    pub(crate) transcriber: ServiceState,
}

/// Everything [`decide`] looks at. A plain value so the whole table is unit-tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Conditions {
    pub(crate) capture: Support,
    pub(crate) permission_query: Support,
    pub(crate) permission: MicPermission,
    /// A failed open is still backing off.
    pub(crate) backing_off: bool,
    pub(crate) ready: Readiness,
    /// A take is open or opening, or a device job is in flight.
    pub(crate) busy: bool,
}

/// The consent decision. Holding a `Start` is condition 1 (the type makes it impossible to ask
/// without one); the rest is checked here, in the order the person can act on.
pub(crate) fn decide(_start: &Start, c: Conditions) -> Result<(), CaptureRefusal> {
    if !c.capture.is_available() || c.backing_off {
        return Err(CaptureRefusal::NoMicrophone);
    }
    permission_allows(c.permission_query, c.permission)?;
    if !c.ready.link_ready {
        return Err(CaptureRefusal::NotReady);
    }
    if !matches!(
        c.ready.transcriber,
        ServiceState::Up | ServiceState::Degraded
    ) {
        return Err(CaptureRefusal::NoTranscriber);
    }
    if c.busy {
        return Err(CaptureRefusal::Busy);
    }
    Ok(())
}

/// Only `Granted`, or `Unknown` on a platform without a permission model, may touch the device.
fn permission_allows(query: Support, p: MicPermission) -> Result<(), CaptureRefusal> {
    match p {
        MicPermission::Granted => Ok(()),
        // No permission model at all (Linux): opening cannot make an OS prompt appear.
        MicPermission::Unknown if !query.is_available() => Ok(()),
        // Unknown on a platform that has a model fails closed, like NotDetermined.
        MicPermission::NotDetermined | MicPermission::Unknown => {
            Err(CaptureRefusal::PermissionNotDetermined)
        }
        _ => Err(CaptureRefusal::PermissionDenied),
    }
}

/// The ONLY spawn of blocking work in fogar-core. Review M2 (architecture owner's decision): a
/// TEMPORARY exception to "no blocking work in core" until the audio port becomes async in the
/// adapter (issue #31); then this function and its `allow` go away. Every device call of the
/// two audio gates (open and drop of a capture here; open and drop of a playback in
/// `playback.rs`, S2 3c) goes through here and nothing else may.
#[allow(clippy::disallowed_methods)] // M2 exception, issue #31 (see above).
pub(crate) fn spawn_device_job<T: Send + 'static>(
    jobs: &mut JoinSet<T>,
    job: impl FnOnce() -> T + Send + 'static,
) -> AbortHandle {
    jobs.spawn_blocking(job)
}

/// Why a take is being closed by something other than the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cut {
    /// The connection that opened it closed or left the conversation.
    OwnerGone,
    /// The adapter ended the chunk stream (device lost, fatal error).
    DeviceEnded,
    /// Nothing new heard for `capture_silence_cut`, or the gateway's second silence warning.
    Silence,
    /// `capture_safety_cut` reached.
    Safety,
}

impl Cut {
    /// What happens to the transcript: an automatic cut never sends.
    pub(crate) fn then(self) -> CaptureThen {
        match self {
            Cut::OwnerGone => CaptureThen::Discard,
            Cut::DeviceEnded | Cut::Silence | Cut::Safety => CaptureThen::Review,
        }
    }
    pub(crate) fn notice(self) -> Option<Notice> {
        match self {
            Cut::Silence => Some(Notice::SilenceCut),
            Cut::Safety => Some(Notice::SafetyCut),
            Cut::OwnerGone | Cut::DeviceEnded => None,
        }
    }
}

pub(crate) type Reply = oneshot::Sender<Result<(), CaptureRefusal>>;
/// Receiving half of the take's owner channel (the IPC connection keeps the sender).
/// Never carries a value (review B1): only its closing matters.
pub(crate) type Owner = oneshot::Receiver<Infallible>;

/// What the engine must act on after [`CaptureGate::next`].
#[derive(Debug)]
pub(crate) enum GateEvent {
    /// The device is open and the take is now open: the engine starts the utterance, publishes
    /// the recording view and only THEN answers `reply` (issue #26).
    Opened {
        reply: Reply,
    },
    /// The open failed or timed out: answer the request.
    Refused {
        reply: Reply,
        why: CaptureRefusal,
    },
    Chunk(AudioChunk),
    /// The take must close (the engine calls [`CaptureGate::close`]).
    Cut(Cut),
    /// The answer to an explicit permission request, for every waiter.
    Permission(MicPermission, Vec<oneshot::Sender<MicPermission>>),
    /// Internal bookkeeping only (a device job finished).
    Nothing,
}

/// A take the gate closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Closed {
    pub(crate) duration: Duration,
    pub(crate) gesture: CaptureGesture,
}

type Opened = (Box<dyn ActiveCapture>, EventStream<AudioChunk>);

/// Result of a blocking device job.
enum Job {
    Opened(Result<Opened, AudioError>),
    Released,
}

struct OpenTake {
    capture: Box<dyn ActiveCapture>,
    stream: EventStream<AudioChunk>,
    owner: Owner,
    gesture: CaptureGesture,
    started: Mono,
    /// Last time something new was heard (open time until the first partial).
    heard_at: Mono,
    /// Last non-empty partial (only to tell a new one from a repeat).
    last_partial: String,
    /// Gateway silence warnings (`status transcriber degraded` without a code) in this take.
    silence_warnings: u8,
}

enum Take {
    Idle,
    Opening {
        owner: Owner,
        reply: Reply,
        gesture: CaptureGesture,
        since: Mono,
        /// `None` while a previous take's release still runs: the open starts when it ends
        /// (the device is never opened while it is still closing). The handle aborts an open
        /// that has not started yet when nobody waits for it any more (review B3).
        job: Option<AbortHandle>,
    },
    Open(Box<OpenTake>),
}

struct PermissionAsk {
    answer: EventStream<MicPermission>,
    waiters: Vec<oneshot::Sender<MicPermission>>,
}

/// What woke [`CaptureGate::next`]; handled after the `select!` releases its borrows.
enum Woke {
    Job(Result<(tokio::task::Id, Job), tokio::task::JoinError>),
    Chunk(Option<AudioChunk>),
    OwnerGone,
    Deadline,
    Permission(Option<MicPermission>),
}

/// The single owner of the microphone. Lives inside the engine task (no locks).
pub(crate) struct CaptureGate {
    mic: Arc<dyn AudioSource>,
    stats: Arc<Stats>,
    silence_cut: Duration,
    safety_cut: Duration,
    format: AudioFormat,
    /// Blocking open/release jobs (`queues::CAPTURE_JOBS`).
    jobs: JoinSet<Job>,
    /// Ids of the open jobs in flight (the current one, or a late one past its deadline).
    opens: Vec<tokio::task::Id>,
    /// Release jobs in flight.
    releasing: usize,
    take: Take,
    open_failures: u32,
    retry_at: Option<Mono>,
    permission: Option<PermissionAsk>,
}

impl std::fmt::Debug for CaptureGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let take = match &self.take {
            Take::Idle => "idle",
            Take::Opening { .. } => "opening",
            Take::Open(_) => "open",
        };
        f.debug_struct("CaptureGate")
            .field("take", &take)
            .field("jobs", &self.jobs.len())
            .field("open_failures", &self.open_failures)
            .finish_non_exhaustive()
    }
}

impl CaptureGate {
    /// Takes the microphone port from the composition root. Touches nothing: no open, no prompt.
    pub(crate) fn new(mic: Arc<dyn AudioSource>, cfg: &Config, stats: Arc<Stats>) -> Self {
        Self {
            mic,
            stats,
            silence_cut: cfg.capture_silence_cut,
            safety_cut: cfg.capture_safety_cut,
            format: AudioFormat::mono(cfg.upload_rate_hz),
            jobs: JoinSet::new(),
            opens: Vec::new(),
            releasing: 0,
            take: Take::Idle,
            open_failures: 0,
            retry_at: None,
            permission: None,
        }
    }

    /// The permission STATUS (never prompts), for the view.
    pub(crate) fn permission(&self) -> MicPermission {
        self.mic.mic_permission()
    }

    pub(crate) fn is_open(&self) -> bool {
        matches!(self.take, Take::Open(_))
    }

    /// A take is open or about to be (the person pressed and the device is opening).
    pub(crate) fn is_active(&self) -> bool {
        !matches!(self.take, Take::Idle)
    }

    fn busy(&self) -> bool {
        !matches!(self.take, Take::Idle) || self.opens.len() >= queues::CAPTURE_JOBS
    }

    /// The consent point. Decides, and on success starts the device open off the runtime; the
    /// answer comes later as [`GateEvent::Opened`] / [`GateEvent::Refused`]. On refusal `reply`
    /// is answered at once and nothing touches the device.
    pub(crate) fn start(
        &mut self,
        start: Start,
        ready: Readiness,
        owner: Owner,
        reply: Reply,
        now: Mono,
    ) {
        let caps = self.mic.capabilities();
        let conditions = Conditions {
            capture: caps.capture,
            permission_query: caps.permission_query,
            permission: self.mic.mic_permission(),
            backing_off: self.retry_at.is_some_and(|t| now < t),
            ready,
            busy: self.busy(),
        };
        if let Err(why) = decide(&start, conditions) {
            self.count_refusal(why);
            let _ = reply.send(Err(why));
            return;
        }
        // Right after a take ended its release may still run: open once it is done.
        let job = (self.releasing == 0).then(|| self.spawn_open());
        self.take = Take::Opening {
            owner,
            reply,
            gesture: start.gesture(),
            since: now,
            job,
        };
    }

    fn spawn_open(&mut self) -> AbortHandle {
        let mic = self.mic.clone();
        Stats::inc(&self.stats.capture_audio_jobs);
        let handle = spawn_device_job(&mut self.jobs, move || {
            Job::Opened(mic.open(queues::MIC_CHUNKS))
        });
        self.opens.push(handle.id());
        handle
    }

    fn count_refusal(&self, why: CaptureRefusal) {
        let s = &self.stats;
        Stats::inc(match why {
            CaptureRefusal::NotSubscribed => &s.captures_refused_unsubscribed,
            CaptureRefusal::PermissionNotDetermined | CaptureRefusal::PermissionDenied => {
                &s.captures_refused_permission
            }
            CaptureRefusal::NotReady | CaptureRefusal::NoTranscriber => {
                &s.captures_refused_not_ready
            }
            CaptureRefusal::Busy => &s.captures_refused_busy,
            _ => &s.captures_refused_unavailable,
        });
    }

    /// The person asked to allow the microphone (IPC `RequestMicPermission`, same-signer peers
    /// only). The ONLY call of the port's prompt in the agent (archtest R13). Never blocks: the
    /// answer arrives as [`GateEvent::Permission`].
    pub(crate) fn ask_permission(&mut self, reply: oneshot::Sender<MicPermission>) {
        if let Some(ask) = &mut self.permission {
            ask.waiters.retain(|w| !w.is_closed());
            if ask.waiters.len() < PERMISSION_WAITERS {
                ask.waiters.push(reply);
            } else {
                let _ = reply.send(self.mic.mic_permission());
            }
            return;
        }
        Stats::inc(&self.stats.mic_permission_requests);
        match self.mic.request_mic_permission() {
            Ok(answer) => {
                self.permission = Some(PermissionAsk {
                    answer,
                    waiters: vec![reply],
                })
            }
            Err(_) => {
                let _ = reply.send(self.mic.mic_permission());
            }
        }
    }

    /// A `transcription_partial` during the take. Only a non-empty partial that differs from the
    /// previous one counts as "heard something new" (GUESSED: whether the `fogar` transcriber
    /// repeats or empties partials in silence is not measured yet; repeats must not keep a
    /// forgotten take alive).
    pub(crate) fn on_partial(&mut self, text: &str, now: Mono) {
        if let Take::Open(t) = &mut self.take {
            let text = text.trim();
            if !text.is_empty() && text != t.last_partial {
                t.last_partial.clear();
                t.last_partial.push_str(text);
                t.heard_at = now;
            }
        }
    }

    /// A gateway silence warning (`status transcriber degraded`, no `code`) during the take.
    /// `true` = the second one: cut now (the third makes the gateway close the session).
    pub(crate) fn on_silence_warning(&mut self) -> bool {
        match &mut self.take {
            Take::Open(t) => {
                t.silence_warnings = t.silence_warnings.saturating_add(1);
                t.silence_warnings >= 2
            }
            _ => false,
        }
    }

    /// Closes whatever take there is. An open take's device is released off the runtime and its
    /// duration returned. A take still opening is abandoned (`opening` answers its request; the
    /// open's late result is released).
    pub(crate) fn close(&mut self, now: Mono, opening: CaptureRefusal) -> Option<Closed> {
        match std::mem::replace(&mut self.take, Take::Idle) {
            Take::Idle => None,
            Take::Opening { reply, job, .. } => {
                // Abandoned before the device opened (review B2): counted, no backoff (a
                // failure of the device is not what happened); an open not started yet is
                // aborted, a running one is released when it comes back.
                if let Some(job) = job {
                    job.abort();
                }
                Stats::inc(&self.stats.captures_discarded);
                let _ = reply.send(Err(opening));
                None
            }
            Take::Open(t) => {
                let OpenTake {
                    capture,
                    stream,
                    gesture,
                    started,
                    ..
                } = *t;
                self.release(capture, stream);
                Some(Closed {
                    duration: now.saturating_since(started),
                    gesture,
                })
            }
        }
    }

    /// Releases a device handle on the blocking pool. Never refused: a release must happen.
    fn release(&mut self, capture: Box<dyn ActiveCapture>, stream: EventStream<AudioChunk>) {
        drop(stream);
        Stats::inc(&self.stats.capture_audio_jobs);
        self.releasing += 1;
        spawn_device_job(&mut self.jobs, move || {
            drop(capture);
            Job::Released
        });
    }

    /// Next instant the gate must act by itself (open deadline, silence and safety cuts). One
    /// timer per take, derived from its state.
    fn deadline(&self) -> Option<(Mono, Cut)> {
        match &self.take {
            Take::Idle => None,
            Take::Opening { since, .. } => Some((since.saturating_add(OPEN_DEADLINE), Cut::Safety)),
            Take::Open(t) => {
                let silence = t.heard_at.saturating_add(self.silence_cut);
                let safety = t.started.saturating_add(self.safety_cut);
                Some(if safety <= silence {
                    (safety, Cut::Safety)
                } else {
                    (silence, Cut::Silence)
                })
            }
        }
    }

    /// Waits for the next thing the engine must act on. Cancel-safe (a `select!` branch of the
    /// engine): every inner branch is (`JoinSet::join_next_with_id`, `EventStream::next`,
    /// polling a oneshot by `&mut`, a clock sleep).
    pub(crate) async fn next<C: Clock>(&mut self, clock: &C) -> GateEvent {
        let deadline = self.deadline();
        let woke = {
            let (stream, owner) = match &mut self.take {
                Take::Idle => (None, None),
                Take::Opening { owner, .. } => (None, Some(owner)),
                Take::Open(t) => (Some(&mut t.stream), Some(&mut t.owner)),
            };
            let has_stream = stream.is_some();
            let has_owner = owner.is_some();
            let answer = self.permission.as_mut().map(|p| &mut p.answer);
            let has_answer = answer.is_some();
            let jobs = &mut self.jobs;
            tokio::select! {
                biased;
                Some(j) = jobs.join_next_with_id(), if !jobs.is_empty() => Woke::Job(j),
                _ = async { if let Some(o) = owner { let _ = o.await; } }, if has_owner => Woke::OwnerGone,
                () = sleep_until_opt(clock, deadline.map(|(at, _)| at)), if deadline.is_some() => Woke::Deadline,
                c = async { match stream { Some(s) => s.next().await, None => None } }, if has_stream => Woke::Chunk(c),
                p = async { match answer { Some(a) => a.next().await, None => None } }, if has_answer => Woke::Permission(p),
                // Nothing to wait for (no take, no job, no prompt): idle until the engine's
                // other branches win.
                else => std::future::pending().await,
            }
        };
        let now = clock.now();
        match woke {
            Woke::Job(j) => self.on_job(j, now),
            Woke::Chunk(Some(c)) => GateEvent::Chunk(c),
            Woke::Chunk(None) => GateEvent::Cut(Cut::DeviceEnded),
            Woke::OwnerGone => match std::mem::replace(&mut self.take, Take::Idle) {
                Take::Open(t) => {
                    self.take = Take::Open(t);
                    GateEvent::Cut(Cut::OwnerGone)
                }
                Take::Opening { job, .. } => {
                    // Nobody waits for this take any more (review B3): an open not started yet
                    // is aborted; a running one is released when it comes back.
                    if let Some(job) = job {
                        job.abort();
                    }
                    Stats::inc(&self.stats.captures_discarded);
                    GateEvent::Nothing
                }
                Take::Idle => GateEvent::Nothing,
            },
            Woke::Deadline => match (&self.take, deadline) {
                (Take::Open(_), Some((_, cut))) => GateEvent::Cut(cut),
                (Take::Opening { .. }, _) => {
                    let Take::Opening { reply, job, .. } =
                        std::mem::replace(&mut self.take, Take::Idle)
                    else {
                        return GateEvent::Nothing;
                    };
                    // Review B2: back off only if the device was actually being opened (not
                    // while waiting for the previous release); counted either way.
                    if job.is_some() {
                        self.open_failed(now);
                    }
                    self.count_refusal(CaptureRefusal::NoMicrophone);
                    GateEvent::Refused {
                        reply,
                        why: CaptureRefusal::NoMicrophone,
                    }
                }
                _ => GateEvent::Nothing,
            },
            Woke::Permission(p) => {
                let waiters = self
                    .permission
                    .take()
                    .map(|a| a.waiters)
                    .unwrap_or_default();
                // The adapter dropped its end without answering: the status as it stands.
                let p = p.unwrap_or_else(|| self.mic.mic_permission());
                GateEvent::Permission(p, waiters)
            }
        }
    }

    fn open_failed(&mut self, now: Mono) {
        self.open_failures = self.open_failures.saturating_add(1);
        let factor = 1u32
            .checked_shl(self.open_failures.saturating_sub(1).min(16))
            .unwrap_or(u32::MAX);
        let delay = OPEN_BACKOFF_BASE
            .saturating_mul(factor)
            .min(OPEN_BACKOFF_CAP);
        self.retry_at = Some(now.saturating_add(delay));
    }

    fn on_job(
        &mut self,
        j: Result<(tokio::task::Id, Job), tokio::task::JoinError>,
        now: Mono,
    ) -> GateEvent {
        let id = match &j {
            Ok((id, _)) => *id,
            Err(e) => e.id(),
        };
        let was_open = self.opens.iter().position(|o| *o == id);
        let result = match (j, was_open) {
            (Ok((_, Job::Opened(r))), _) => r.map_err(Some),
            // A panicking adapter during an open: a failed open.
            (Err(_), Some(_)) => Err(None),
            // A release finished (or panicked: the handle is gone either way). A take waiting
            // for it may open now.
            (Ok((_, Job::Released)), _) | (Err(_), None) => {
                self.releasing = self.releasing.saturating_sub(1);
                if self.releasing > 0 || !matches!(self.take, Take::Opening { job: None, .. }) {
                    return GateEvent::Nothing;
                }
                // Review F3: the permission may have changed while waiting for the release.
                let caps = self.mic.capabilities();
                if let Err(why) =
                    permission_allows(caps.permission_query, self.mic.mic_permission())
                {
                    let Take::Opening { reply, .. } = std::mem::replace(&mut self.take, Take::Idle)
                    else {
                        return GateEvent::Nothing;
                    };
                    self.count_refusal(why);
                    return GateEvent::Refused { reply, why };
                }
                let handle = self.spawn_open();
                if let Take::Opening { job, .. } = &mut self.take {
                    *job = Some(handle);
                }
                return GateEvent::Nothing;
            }
        };
        if let Some(i) = was_open {
            self.opens.swap_remove(i);
        }
        let ours = matches!(&self.take, Take::Opening { job: Some(job), .. } if job.id() == id);
        if !ours {
            // A late open nobody waits for: release it at once, never use it.
            if let Ok((capture, stream)) = result {
                self.release(capture, stream);
            }
            return GateEvent::Nothing;
        }
        let Take::Opening {
            owner,
            reply,
            gesture,
            ..
        } = std::mem::replace(&mut self.take, Take::Idle)
        else {
            return GateEvent::Nothing;
        };
        let (capture, stream) = match result {
            Ok(opened) => opened,
            Err(e) => {
                self.open_failed(now);
                let why = match e {
                    Some(AudioError::PermissionDenied) => CaptureRefusal::PermissionDenied,
                    _ => CaptureRefusal::NoMicrophone,
                };
                self.count_refusal(why);
                return GateEvent::Refused { reply, why };
            }
        };
        if capture.format() != self.format {
            // v1 uploads f32le 16 kHz mono only; the adapter must resample.
            self.release(capture, stream);
            self.open_failed(now);
            self.count_refusal(CaptureRefusal::NoMicrophone);
            return GateEvent::Refused {
                reply,
                why: CaptureRefusal::NoMicrophone,
            };
        }
        let mut owner = owner;
        let owner_gone = matches!(owner.try_recv(), Err(oneshot::error::TryRecvError::Closed));
        if reply.is_closed() || owner_gone {
            // The request was given up (and answered "busy"), or its connection left, while the
            // device opened: nobody would hear this take.
            self.release(capture, stream);
            Stats::inc(&self.stats.captures_discarded);
            return GateEvent::Nothing;
        }
        self.open_failures = 0;
        self.retry_at = None;
        Stats::inc(&self.stats.captures_opened);
        self.take = Take::Open(Box::new(OpenTake {
            capture,
            stream,
            owner,
            gesture,
            started: now,
            heard_at: now,
            last_partial: String::new(),
            silence_warnings: 0,
        }));
        GateEvent::Opened { reply }
    }

    /// Agent stop: closes any take and waits (bounded) for the device jobs, then leaves what is
    /// still running to the blocking pool (each job is bounded by the adapter's own budget).
    pub(crate) async fn shutdown<C: Clock>(&mut self, clock: &C) {
        self.close(clock.now(), CaptureRefusal::NotReady);
        self.permission = None;
        let jobs = &mut self.jobs;
        let drained = crate::time::deadline(clock, RELEASE_WAIT_ON_STOP, async {
            while let Some(j) = jobs.join_next().await {
                // A late open still releases its device, on the blocking pool.
                if let Ok(Job::Opened(Ok((capture, _stream)))) = j {
                    spawn_device_job(jobs, move || {
                        drop(capture);
                        Job::Released
                    });
                }
            }
        })
        .await;
        if drained.is_none() {
            self.jobs.detach_all();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]
    use super::*;
    use fogar_ports::{
        audio::{AudioSourceCaps, MicPermission as P},
        events::event_channel,
    };
    use fogar_testkit::{FakeMic, TestClock};
    use std::{
        sync::{Mutex, mpsc as std_mpsc},
        thread::ThreadId,
    };

    const READY: Readiness = Readiness {
        link_ready: true,
        transcriber: ServiceState::Up,
    };

    fn act() -> Start {
        Start::UserAct(UserAct::for_test(CaptureGesture::Hold))
    }

    fn ok_conditions() -> Conditions {
        Conditions {
            capture: Support::Native,
            permission_query: Support::Native,
            permission: P::Granted,
            backing_off: false,
            ready: READY,
            busy: false,
        }
    }

    /// Spec S2: «tabla unitaria de apertura con todas las combinaciones de condiciones».
    #[test]
    fn decision_table_covers_every_combination() {
        use CaptureRefusal as R;
        let perms = [
            P::Granted,
            P::Denied,
            P::NotDetermined,
            P::Restricted,
            P::Unknown,
        ];
        let services = [
            ServiceState::Up,
            ServiceState::Degraded,
            ServiceState::Down,
            ServiceState::Unknown,
        ];
        let sup = [Support::Native, Support::Unsupported];
        let mut opened = 0;
        for capture in sup {
            for permission_query in sup {
                for permission in perms {
                    for backing_off in [false, true] {
                        for link_ready in [true, false] {
                            for transcriber in services {
                                for busy in [false, true] {
                                    let c = Conditions {
                                        capture,
                                        permission_query,
                                        permission,
                                        backing_off,
                                        ready: Readiness {
                                            link_ready,
                                            transcriber,
                                        },
                                        busy,
                                    };
                                    let perm_ok = permission == P::Granted
                                        || (permission == P::Unknown
                                            && permission_query == Support::Unsupported);
                                    let want = if capture == Support::Unsupported || backing_off {
                                        Err(R::NoMicrophone)
                                    } else if !perm_ok {
                                        Err(match permission {
                                            P::Denied | P::Restricted => R::PermissionDenied,
                                            _ => R::PermissionNotDetermined,
                                        })
                                    } else if !link_ready {
                                        Err(R::NotReady)
                                    } else if !matches!(
                                        transcriber,
                                        ServiceState::Up | ServiceState::Degraded
                                    ) {
                                        Err(R::NoTranscriber)
                                    } else if busy {
                                        Err(R::Busy)
                                    } else {
                                        Ok(())
                                    };
                                    assert_eq!(decide(&act(), c), want, "{c:?}");
                                    opened += usize::from(want.is_ok());
                                }
                            }
                        }
                    }
                }
            }
        }
        // Granted (2 query kinds) + Unknown without a model, x 2 live transcriber states.
        assert_eq!(opened, 6);
        assert_eq!(decide(&act(), ok_conditions()), Ok(()));
    }

    fn gate(mic: Arc<dyn AudioSource>) -> (CaptureGate, Arc<Stats>) {
        let stats = Arc::new(Stats::default());
        (
            CaptureGate::new(mic, &Config::default(), stats.clone()),
            stats,
        )
    }

    /// Starts a take whose owner stays alive (the returned sender) and returns its answer.
    async fn start(
        g: &mut CaptureGate,
        clock: &TestClock,
    ) -> (
        oneshot::Sender<Infallible>,
        oneshot::Receiver<Result<(), CaptureRefusal>>,
    ) {
        let (owner_tx, owner) = oneshot::channel();
        let (reply, answer) = oneshot::channel();
        g.start(act(), READY, owner, reply, clock.now());
        (owner_tx, answer)
    }

    #[derive(Debug, Default)]
    struct Seen {
        open_thread: Option<ThreadId>,
        drop_thread: Option<ThreadId>,
    }

    /// A microphone whose `open` blocks until the test lets it go (the macOS adapter may block
    /// up to `OPEN_BLOCK_MAX`) and records the threads that open and drop it.
    #[derive(Debug)]
    struct BlockingMic {
        release: Mutex<std_mpsc::Receiver<()>>,
        seen: Arc<Mutex<Seen>>,
    }

    #[derive(Debug)]
    struct SeenCapture(Arc<Mutex<Seen>>);

    impl ActiveCapture for SeenCapture {
        fn format(&self) -> AudioFormat {
            AudioFormat::mono(16_000)
        }
    }

    impl Drop for SeenCapture {
        fn drop(&mut self) {
            self.0.lock().unwrap().drop_thread = Some(std::thread::current().id());
        }
    }

    impl AudioSource for BlockingMic {
        fn capabilities(&self) -> AudioSourceCaps {
            let mut c = AudioSourceCaps::NONE;
            c.capture = Support::Native;
            c.permission_query = Support::Native;
            c
        }
        fn mic_permission(&self) -> P {
            P::Granted
        }
        fn open(
            &self,
            capacity: std::num::NonZeroUsize,
        ) -> Result<(Box<dyn ActiveCapture>, EventStream<AudioChunk>), AudioError> {
            self.seen.lock().unwrap().open_thread = Some(std::thread::current().id());
            // Blocks this thread like a slow CoreAudio start would.
            let _ = self.release.lock().unwrap().recv();
            let (_tx, rx) = event_channel(capacity);
            Ok((Box::new(SeenCapture(self.seen.clone())), rx))
        }
    }

    /// Issue #24: open and drop run off the async runtime. The test runs on a current-thread
    /// runtime (like fogar-agentd): if `open` ran on it, the test task could never send the
    /// release below and the gate would never answer.
    #[tokio::test(flavor = "current_thread")]
    async fn open_and_release_happen_off_the_runtime_thread() {
        let runtime_thread = std::thread::current().id();
        let (release_tx, release_rx) = std_mpsc::channel();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let (mut g, stats) = gate(Arc::new(BlockingMic {
            release: Mutex::new(release_rx),
            seen: seen.clone(),
        }));
        let clock = TestClock::new();
        let (_owner, mut answer) = start(&mut g, &clock).await;
        // The runtime is free while the device opens: this task keeps running.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(answer.try_recv().is_err(), "still opening");
        release_tx.send(()).unwrap();
        match g.next(&clock).await {
            GateEvent::Opened { reply } => reply.send(Ok(())).unwrap(),
            other => panic!("{other:?}"),
        }
        assert!(g.is_open());
        assert!(g.close(clock.now(), CaptureRefusal::Busy).is_some());
        // The release job finishes on the blocking pool.
        assert!(matches!(g.next(&clock).await, GateEvent::Nothing));
        let seen = seen.lock().unwrap();
        assert_ne!(seen.open_thread, Some(runtime_thread));
        assert_ne!(seen.drop_thread, Some(runtime_thread));
        assert!(seen.open_thread.is_some() && seen.drop_thread.is_some());
        assert_eq!(stats.snapshot().capture_audio_jobs, 2);
    }

    /// An open that outlives `OPEN_DEADLINE` is refused, backs off, and its late handle is
    /// released (never used).
    #[tokio::test(start_paused = true)]
    async fn a_late_open_is_refused_then_released_and_backs_off() {
        let (release_tx, release_rx) = std_mpsc::channel();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let (mut g, _) = gate(Arc::new(BlockingMic {
            release: Mutex::new(release_rx),
            seen: seen.clone(),
        }));
        let clock = TestClock::new();
        let (_owner, _answer) = start(&mut g, &clock).await;
        // A running blocking job stops tokio's auto-advance: move the paused clock by hand.
        let (ev, ()) = tokio::join!(g.next(&clock), async {
            tokio::task::yield_now().await;
            tokio::time::advance(OPEN_DEADLINE + Duration::from_millis(1)).await;
        });
        match ev {
            GateEvent::Refused { why, .. } => assert_eq!(why, CaptureRefusal::NoMicrophone),
            other => panic!("{other:?}"),
        }
        // Still busy while the stuck open runs; and backing off afterwards.
        let (_o, mut busy) = start(&mut g, &clock).await;
        assert_eq!(busy.try_recv().unwrap(), Err(CaptureRefusal::NoMicrophone));
        release_tx.send(()).unwrap();
        assert!(
            matches!(g.next(&clock).await, GateEvent::Nothing),
            "late open"
        );
        assert!(
            matches!(g.next(&clock).await, GateEvent::Nothing),
            "released"
        );
        assert!(seen.lock().unwrap().drop_thread.is_some());
        assert!(!g.is_open());
    }

    #[tokio::test(start_paused = true)]
    async fn new_partials_keep_a_take_alive_and_repeats_do_not() {
        let mic = FakeMic::healthy();
        let (mut g, _) = gate(Arc::new(mic.clone()));
        let clock = TestClock::new();
        let (_owner, _answer) = start(&mut g, &clock).await;
        let GateEvent::Opened { reply } = g.next(&clock).await else {
            panic!()
        };
        reply.send(Ok(())).unwrap();
        let t0 = clock.now();
        tokio::time::sleep(Duration::from_secs(4)).await;
        g.on_partial("pon", clock.now());
        tokio::time::sleep(Duration::from_secs(4)).await;
        g.on_partial("pon", clock.now()); // a repeat: nothing new
        g.on_partial("  ", clock.now()); // empty: nothing new
        match g.next(&clock).await {
            GateEvent::Cut(c) => assert_eq!(c, Cut::Silence),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            clock.now().saturating_since(t0),
            Duration::from_secs(9),
            "5 s after the last NEW partial"
        );
        assert!(mic.is_open(), "the engine closes it");
        assert!(g.close(clock.now(), CaptureRefusal::Busy).is_some());
        assert!(
            matches!(g.next(&clock).await, GateEvent::Nothing),
            "the release job"
        );
        assert!(!mic.is_open(), "the device is released");
        assert!(!g.is_active() && !g.busy());
    }

    /// A new take right after the previous one ended waits for that release instead of being
    /// refused `Busy` (or opening the device while it still closes).
    #[tokio::test(start_paused = true)]
    async fn a_take_right_after_another_waits_for_the_release() {
        let mic = FakeMic::healthy();
        let (mut g, _) = gate(Arc::new(mic.clone()));
        let clock = TestClock::new();
        let (_o1, _a1) = start(&mut g, &clock).await;
        let GateEvent::Opened { reply } = g.next(&clock).await else {
            panic!()
        };
        reply.send(Ok(())).unwrap();
        assert!(g.close(clock.now(), CaptureRefusal::Busy).is_some());
        let (_o2, mut a2) = start(&mut g, &clock).await;
        assert!(
            a2.try_recv().is_err(),
            "not refused: waiting for the release"
        );
        assert_eq!(mic.opens(), 1, "no open while the device closes");
        assert!(
            matches!(g.next(&clock).await, GateEvent::Nothing),
            "released"
        );
        match g.next(&clock).await {
            GateEvent::Opened { reply } => reply.send(Ok(())).unwrap(),
            other => panic!("{other:?}"),
        }
        assert_eq!(mic.opens(), 2);
        assert!(g.is_open() && mic.is_open());
    }

    #[tokio::test(start_paused = true)]
    async fn the_second_silence_warning_cuts_and_cuts_never_send() {
        let (mut g, _) = gate(Arc::new(FakeMic::healthy()));
        let clock = TestClock::new();
        assert!(!g.on_silence_warning(), "no take: nothing to cut");
        let (_owner, _answer) = start(&mut g, &clock).await;
        let GateEvent::Opened { reply } = g.next(&clock).await else {
            panic!()
        };
        reply.send(Ok(())).unwrap();
        assert!(!g.on_silence_warning());
        assert!(g.on_silence_warning());
        for cut in [Cut::Silence, Cut::Safety, Cut::DeviceEnded, Cut::OwnerGone] {
            assert_ne!(cut.then(), CaptureThen::Send, "{cut:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_capture_at_another_rate_is_refused_and_released() {
        let mic = FakeMic::healthy().with_format(AudioFormat::mono(48_000));
        let (mut g, _) = gate(Arc::new(mic.clone()));
        let clock = TestClock::new();
        let (_owner, _answer) = start(&mut g, &clock).await;
        match g.next(&clock).await {
            GateEvent::Refused { why, .. } => assert_eq!(why, CaptureRefusal::NoMicrophone),
            other => panic!("{other:?}"),
        }
        assert!(matches!(g.next(&clock).await, GateEvent::Nothing));
        assert!(!mic.is_open());
    }

    /// Branch pinned: the connection leaves while `open` is still blocked, so the gate sees
    /// `OwnerGone` first (abandons the take); the open's late result is then released.
    #[tokio::test(flavor = "current_thread")]
    async fn an_owner_gone_while_opening_never_records() {
        let (release_tx, release_rx) = std_mpsc::channel();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let (mut g, stats) = gate(Arc::new(BlockingMic {
            release: Mutex::new(release_rx),
            seen: seen.clone(),
        }));
        let clock = TestClock::new();
        let (owner, _answer) = start(&mut g, &clock).await;
        // The open is running (blocked in the device), so B3's abort cannot cancel it.
        while seen.lock().unwrap().open_thread.is_none() {
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(owner);
        assert!(
            matches!(g.next(&clock).await, GateEvent::Nothing),
            "owner gone"
        );
        assert!(!g.is_active());
        assert_eq!(stats.snapshot().captures_discarded, 1);
        release_tx.send(()).unwrap();
        assert!(
            matches!(g.next(&clock).await, GateEvent::Nothing),
            "late open"
        );
        assert!(
            matches!(g.next(&clock).await, GateEvent::Nothing),
            "released"
        );
        assert!(seen.lock().unwrap().drop_thread.is_some());
        assert!(!g.is_open() && !g.busy());
        assert_eq!(stats.snapshot().captures_opened, 0);
    }

    /// Branch pinned: the open already finished when the gate looks, and its connection is
    /// gone: the job branch wins (biased) and the handle is released, never used.
    #[tokio::test(flavor = "current_thread")]
    async fn an_open_finished_after_its_owner_left_is_released() {
        let mic = FakeMic::healthy();
        let (mut g, stats) = gate(Arc::new(mic.clone()));
        let clock = TestClock::new();
        let (owner, _answer) = start(&mut g, &clock).await;
        drop(owner);
        // Let the blocking pool finish the (instant) open before the gate polls.
        while mic.opens() == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            matches!(g.next(&clock).await, GateEvent::Nothing),
            "open done"
        );
        assert!(
            matches!(g.next(&clock).await, GateEvent::Nothing),
            "released"
        );
        assert!(!mic.is_open());
        let s = stats.snapshot();
        assert_eq!((s.captures_opened, s.captures_discarded), (0, 1));
    }

    /// Review F3: the permission is read again when a start that waited for the previous
    /// release is about to touch the device.
    #[tokio::test(start_paused = true)]
    async fn a_start_waiting_for_the_release_rechecks_the_permission() {
        let mic = FakeMic::healthy();
        let (mut g, _) = gate(Arc::new(mic.clone()));
        let clock = TestClock::new();
        let (_o1, _a1) = start(&mut g, &clock).await;
        let GateEvent::Opened { reply } = g.next(&clock).await else {
            panic!()
        };
        reply.send(Ok(())).unwrap();
        assert!(g.close(clock.now(), CaptureRefusal::Busy).is_some());
        let (_o2, _a2) = start(&mut g, &clock).await;
        mic.set_permission(P::Denied); // revoked in System Settings meanwhile
        match g.next(&clock).await {
            GateEvent::Refused { why, .. } => assert_eq!(why, CaptureRefusal::PermissionDenied),
            other => panic!("{other:?}"),
        }
        assert_eq!(mic.opens(), 1);
        assert!(!g.is_active());
    }
}
