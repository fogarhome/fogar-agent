//! The engine task: IPC commands (typed text, cancel) -> turn machine -> uplink; inbound events
//! -> turn machine + conversation view -> playback. Single task, single owner of all turn and
//! conversation state (no locks). Every await is either a cancel-safe `select!` branch or
//! bounded by a deadline.
//!
//! S2 Decision 1: the microphone is never opened at start. The engine does not hold the
//! microphone at all: it owns the consent gate (`capture.rs`), the single point that can open a
//! take, and only on a `Command::CaptureStart` carrying the person's `UserAct`. Gateway events
//! can end a take (silence warnings, a dropped link) but never start one.
//!
//! S2 3c: the engine does not hold the speaker either. TTS goes through the playback gate
//! (`playback.rs`), the single output point: a turn's audio plays only once the app confirmed
//! the turn is on screen (`Command::TurnVisible`), within 2 s of its `turn_start`.

use crate::{
    capture::{CaptureGate, Cut, GateEvent, Owner, Readiness, Reply, Start},
    config::Config,
    conversation,
    link::Inbound,
    playback::PlaybackGate,
    stats::Stats,
    time::{deadline, sleep_until_opt},
    turn::{ClientCmd, Effects, TurnMachine, UserPhase},
};
use fogar_ports::{
    audio::MicPermission,
    clock::Clock,
    gateway::Frame,
    ipc::{
        CaptureRefusal, CaptureThen, ConversationView, Notice, TextRefusal, TurnOutcome,
        UserActivity,
    },
};
use fogar_proto::{
    ClientMsg, encode_client, encode_upload_f32le, parse_tts_frame,
    server::{ServerMsg, Service, ServiceState as WireServiceState},
};
use std::{borrow::Cow, sync::Arc};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

/// IPC -> engine (`queues::ENGINE_COMMANDS`). Each carries the reply channel; the IPC side waits
/// for it under `write_timeout` and drops it on expiry, and the engine then skips the command
/// (`is_closed`) instead of acting on a request the UI was told failed.
pub(crate) enum Command {
    SendText(String, oneshot::Sender<Result<(), TextRefusal>>),
    Cancel(oneshot::Sender<()>),
    /// The person pressed the microphone button (only `ipc.rs` can build the `Start`). `owner`
    /// closes when the connection that asked goes away.
    CaptureStart {
        start: Start,
        owner: Owner,
        reply: Reply,
    },
    CaptureEnd(CaptureThen, oneshot::Sender<()>),
    RequestMicPermission(oneshot::Sender<MicPermission>),
    /// The app shows this turn (its id as the view carries it): its audio may play.
    TurnVisible(String, oneshot::Sender<()>),
}

/// Redacting `Debug`: the text is the user's.
impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::SendText(t, _) => write!(f, "SendText(<redacted {} bytes>)", t.len()),
            Command::Cancel(_) => f.write_str("Cancel"),
            Command::CaptureStart { start, .. } => write!(f, "CaptureStart({start:?})"),
            Command::CaptureEnd(then, _) => write!(f, "CaptureEnd({then:?})"),
            Command::RequestMicPermission(_) => f.write_str("RequestMicPermission"),
            Command::TurnVisible(id, _) => write!(f, "TurnVisible({} bytes)", id.len()),
        }
    }
}

pub(crate) struct Engine<C> {
    pub(crate) clock: Arc<C>,
    pub(crate) cfg: Arc<Config>,
    pub(crate) stats: Arc<Stats>,
    pub(crate) out_tx: mpsc::Sender<Frame>,
    pub(crate) in_rx: mpsc::Receiver<Inbound>,
    pub(crate) cmd_rx: mpsc::Receiver<Command>,
    pub(crate) conv: watch::Sender<ConversationView>,
}

struct State {
    turns: TurnMachine,
    capture: CaptureGate,
    playback: PlaybackGate,
}

impl State {
    fn playing(&self) -> bool {
        self.playback.playing()
    }
}

impl<C: Clock> Engine<C> {
    /// `capture` is the single consent point, the only holder of the microphone; `playback`
    /// the single output point, the only holder of the speaker. Both live in the task's state
    /// (not in `Engine`, which `&self` methods share across awaits).
    pub(crate) async fn run(
        mut self,
        capture: CaptureGate,
        playback: PlaybackGate,
        ingress: CancellationToken,
    ) {
        let mut st = State {
            turns: TurnMachine::new(self.cfg.turn),
            capture,
            playback,
        };
        self.refresh_mic(&st);
        // No IPC (or it stopped): the closed channel must not turn the branch into a hot loop.
        let mut cmds_open = true;
        loop {
            let wake_at = st.turns.next_deadline();
            tokio::select! {
                biased;
                () = ingress.cancelled() => break,
                // Review M3: the take before the gateway. A gateway that keeps `in_rx` full must
                // never starve the take's cuts (safety, silence, owner gone) or its device jobs;
                // the take's own events are bounded by the device (50 chunks/s) and its timers.
                ev = st.capture.next(&*self.clock) => self.on_capture(&mut st, ev).await,
                // The output's own timers and device jobs (visibility deadline, open, release,
                // drain, idle close) are not starved by a busy gateway either.
                () = st.playback.next(&*self.clock) => {}
                ev = self.in_rx.recv() => match ev {
                    None => break,
                    Some(ev) => self.on_inbound(&mut st, ev).await,
                },
                cmd = self.cmd_rx.recv(), if cmds_open => match cmd {
                    None => cmds_open = false,
                    Some(cmd) => self.on_command(&mut st, cmd).await,
                },
                () = sleep_until_opt(&*self.clock, wake_at), if wake_at.is_some() => {
                    let transcribing = matches!(st.turns.phase(), UserPhase::AwaitTranscription { .. });
                    if let Some(fx) = st.turns.on_tick(self.clock.now()) {
                        if transcribing {
                            Stats::inc(&self.stats.transcriptions_timed_out);
                            self.publish(&st, |_| false);
                        } else {
                            Stats::inc(&self.stats.user_waits_timed_out);
                            self.publish(&st, |v| conversation::wait_ended(v, TurnOutcome::Timeout));
                        }
                        self.apply(&mut st, fx).await;
                    }
                }
            }
            self.follow_turn(&mut st);
        }
        // Agent stop (`ingress`): the take closes, the output goes silent, and both devices are
        // released off the runtime.
        let clock = &*self.clock;
        tokio::join!(st.capture.shutdown(clock), st.playback.shutdown(clock));
    }

    /// The playback gate follows the turn machine's turn (a new turn starts its 2 s wait for
    /// `TurnVisible`), and the view follows the gate.
    fn follow_turn(&self, st: &mut State) {
        let now = self.clock.now();
        st.playback.follow(
            st.turns.turn().map(|t| (t.turn_id.as_str(), t.turn_seq)),
            now,
        );
        let mark = st.playback.take_mark();
        let speaking = st.playback.speaking();
        let awaiting = st.playback.awaiting_visible();
        self.conv.send_if_modified(|v| {
            let m = mark
                .as_ref()
                .is_some_and(|(id, a)| conversation::turn_audio(v, id, *a));
            conversation::playback(v, speaking, awaiting) | m
        });
    }

    /// Applies `f` to the view and publishes it if anything changed (activity follows the turn
    /// machine). The view lives inside the `watch`: no copy per token.
    fn publish(&self, st: &State, f: impl FnOnce(&mut ConversationView) -> bool) {
        let activity = match st.turns.phase() {
            UserPhase::Recording => UserActivity::Recording,
            UserPhase::AwaitTranscription { .. } => UserActivity::Transcribing,
            UserPhase::AwaitTurn { .. } => UserActivity::AwaitingReply,
            UserPhase::Offline | UserPhase::Idle => UserActivity::Idle,
        };
        self.conv
            .send_if_modified(|v| f(v) | conversation::set_activity(v, activity));
    }

    /// The permission STATUS only (the port guarantees it never prompts).
    fn refresh_mic(&self, st: &State) {
        let m = st.capture.permission();
        self.conv.send_if_modified(|v| conversation::set_mic(v, m));
    }

    async fn on_command(&self, st: &mut State, cmd: Command) {
        match cmd {
            Command::SendText(text, reply) => {
                // The IPC gave up (and counted it as busy): never act on it late.
                if reply.is_closed() {
                    return;
                }
                // Also while the device is opening (review B4): the take is already the person's.
                if st.turns.phase() == UserPhase::Recording || st.capture.is_active() {
                    let _ = reply.send(Err(TextRefusal::Recording));
                    return;
                }
                let playing = st.playing();
                let Some(fx) = st.turns.send_text(text, playing, self.clock.now()) else {
                    let _ = reply.send(Err(TextRefusal::NotReady));
                    return;
                };
                let _ = reply.send(Ok(()));
                if let Some(ClientCmd::Send(t)) = &fx.send {
                    self.publish(st, |v| conversation::sent(v, t));
                }
                Stats::inc(&self.stats.texts_sent);
                self.apply(st, fx).await;
            }
            Command::Cancel(reply) => {
                // The IPC gave up (and counted it as busy): never act on it late.
                if reply.is_closed() {
                    return;
                }
                let _ = reply.send(());
                let playing = st.playing();
                let fx = st.turns.cancel(playing, self.clock.now());
                if fx.send.is_some() {
                    Stats::inc(&self.stats.cancels_sent);
                }
                // A wait with no turn yet ends here; a streaming turn ends with its turn_end.
                self.publish(st, |v| conversation::wait_ended(v, TurnOutcome::Cancelled));
                self.apply(st, fx).await;
            }
            Command::CaptureStart {
                start,
                owner,
                reply,
            } => {
                // The IPC gave up (and counted it as busy): never open the microphone late.
                if reply.is_closed() {
                    return;
                }
                let ready = Readiness {
                    link_ready: st.turns.phase() != UserPhase::Offline,
                    transcriber: self.conv.borrow().transcriber,
                };
                st.capture
                    .start(start, ready, owner, reply, self.clock.now());
                // A refusal may come from a permission changed in System Settings.
                self.refresh_mic(st);
            }
            Command::CaptureEnd(then, reply) => {
                // Ending is always safe: done even if the IPC already gave up waiting.
                let _ = reply.send(());
                self.end_take(st, then, None).await;
            }
            Command::RequestMicPermission(reply) => {
                if !reply.is_closed() {
                    st.capture.ask_permission(reply);
                }
            }
            Command::TurnVisible(id, reply) => {
                // The IPC gave up (and counted it as busy): never act on it late.
                if reply.is_closed() {
                    return;
                }
                let _ = reply.send(());
                st.playback.visible(&id, self.clock.now());
            }
        }
    }

    async fn on_capture(&self, st: &mut State, ev: GateEvent) {
        match ev {
            GateEvent::Nothing => {}
            GateEvent::Refused { reply, why } => {
                let _ = reply.send(Err(why));
                self.refresh_mic(st);
            }
            GateEvent::Opened { reply } => {
                let now = self.clock.now();
                let was_waiting = matches!(st.turns.phase(), UserPhase::AwaitTurn { .. });
                let playing = st.playing();
                let Some(fx) = st.turns.utterance_start(playing, now) else {
                    // The link dropped meanwhile (SessionDown closes takes; defensive).
                    st.capture.close(now, CaptureRefusal::NotReady);
                    let _ = reply.send(Err(CaptureRefusal::NotReady));
                    return;
                };
                if fx.send.is_some() {
                    Stats::inc(&self.stats.cancels_sent);
                }
                // Issue #26: the recording view is published BEFORE `CaptureStarted` goes back,
                // so no push of an older view can follow the answer.
                self.publish(st, |v| {
                    let w = was_waiting && conversation::wait_ended(v, TurnOutcome::Cancelled);
                    conversation::take_started(v) | w
                });
                let answered = reply.send(Ok(())).is_ok();
                self.apply(st, fx).await;
                if !answered {
                    // Nobody heard the answer: the microphone must not stay on.
                    self.end_take(st, CaptureThen::Discard, None).await;
                }
            }
            GateEvent::Chunk(c) => {
                Stats::inc(&self.stats.mic_chunks);
                if st.turns.phase() == UserPhase::Recording {
                    self.send_audio(&c.samples);
                }
            }
            GateEvent::Cut(cut) => {
                match cut {
                    Cut::Silence => Stats::inc(&self.stats.captures_silence_cut),
                    Cut::Safety => Stats::inc(&self.stats.captures_safety_cut),
                    Cut::OwnerGone | Cut::DeviceEnded => {}
                }
                self.end_take(st, cut.then(), cut.notice()).await;
            }
            GateEvent::Permission(p, waiters) => {
                for w in waiters {
                    let _ = w.send(p);
                }
                self.refresh_mic(st);
            }
        }
    }

    /// Closes the take (if any): the device is released off the runtime, `end` goes out and
    /// `then` decides what the final transcript becomes. A take shorter than `capture_min` is
    /// discarded whatever was asked.
    async fn end_take(&self, st: &mut State, then: CaptureThen, notice: Option<Notice>) {
        let now = self.clock.now();
        let Some(closed) = st.capture.close(now, CaptureRefusal::Busy) else {
            return;
        };
        let then = if closed.duration < self.cfg.capture_min {
            CaptureThen::Discard
        } else {
            then
        };
        if then == CaptureThen::Discard {
            Stats::inc(&self.stats.captures_discarded);
        }
        let fx = st.turns.utterance_end(then, now);
        self.publish(st, |v| conversation::take_ended(v, notice));
        self.apply(st, fx).await;
    }

    /// Best effort: shed the newest chunk when the uplink queue is full.
    fn send_audio(&self, samples: &[f32]) {
        let mut buf = Vec::with_capacity(samples.len() * 4);
        encode_upload_f32le(samples, &mut buf);
        match self.out_tx.try_send(Frame::Binary(buf)) {
            Ok(()) => Stats::hwm(
                &self.stats.out_queue_hwm,
                self.out_tx.max_capacity() - self.out_tx.capacity(),
            ),
            Err(mpsc::error::TrySendError::Full(_)) => Stats::inc(&self.stats.uplink_audio_shed),
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    async fn on_inbound(&self, st: &mut State, ev: Inbound) {
        match ev {
            Inbound::SessionUp(caps) => {
                st.turns.link_up();
                self.refresh_mic(st);
                self.publish(st, |v| conversation::link_up(v, caps));
            }
            Inbound::SessionDown => {
                // The session that heard the take is gone: close it (no `end` to send).
                let took = st.capture.close(self.clock.now(), CaptureRefusal::NotReady);
                let fx = st.turns.link_down();
                self.publish(st, |v| {
                    let t = took.is_some() && conversation::take_ended(v, None);
                    conversation::link_down(v) | t
                });
                self.apply(st, fx).await;
                // Silence, and release the output device while offline (energy).
                st.playback.link_down(self.clock.now());
            }
            Inbound::Server(msg, _charge) => {
                let now = self.clock.now();
                let mut cut = None;
                if st.turns.phase() == UserPhase::Recording && st.capture.is_open() {
                    match &msg {
                        ServerMsg::TranscriptionPartial(p) => {
                            st.capture.on_partial(&p.text, now);
                            self.conv
                                .send_if_modified(|v| conversation::partial(v, &p.text));
                        }
                        // The gateway's silence watchdog warning (spec S2 «Límites de una
                        // toma»): `degraded` without a code; the second one cuts the take.
                        ServerMsg::Status(s)
                            if s.service() == Service::Transcriber
                                && s.state() == WireServiceState::Degraded
                                && s.code.is_none()
                                && st.capture.on_silence_warning() =>
                        {
                            cut = Some(Cut::Silence);
                        }
                        _ => {}
                    }
                }
                let was_waiting = matches!(st.turns.phase(), UserPhase::AwaitTurn { .. });
                let fx = st.turns.on_server(&msg, now);
                if let Some(ClientCmd::Send(t)) = &fx.send {
                    // A take ended with `Send`: its final transcript is my turn now.
                    self.publish(st, |v| conversation::sent(v, t));
                }
                if let Some(text) = &fx.review {
                    self.publish(st, |v| conversation::draft(v, &text.0));
                }
                if fx.turn_started {
                    Stats::inc(&self.stats.turns_started);
                }
                // `None` for a turn_start the machine ignored (late answer to an abandoned send).
                let origin = st
                    .turns
                    .turn()
                    .filter(|_| fx.turn_started)
                    .map(|t| t.origin);
                self.publish(st, |v| conversation::on_server(v, &msg, origin));
                if was_waiting
                    && !fx.turn_started
                    && !matches!(st.turns.phase(), UserPhase::AwaitTurn { .. })
                {
                    // The machine gave up the wait (a TURN_ERROR): the view's entry follows,
                    // whatever the error named, so the two never disagree.
                    self.publish(st, |v| conversation::wait_ended(v, TurnOutcome::Error));
                }
                self.apply(st, fx).await;
                if let Some(cut) = cut {
                    Stats::inc(&self.stats.captures_silence_cut);
                    self.end_take(st, cut.then(), cut.notice()).await;
                }
            }
            Inbound::Audio(buf, _charge) => {
                let Ok(frame) = parse_tts_frame(&buf) else {
                    Stats::inc(&self.stats.tts_frames_invalid);
                    return;
                };
                if !st.turns.accept_audio(frame.turn_seq) {
                    Stats::inc(&self.stats.tts_frames_stale);
                    return;
                }
                // The gate learns about a turn_start handled in this same loop turn first.
                self.follow_turn(st);
                st.playback.on_frame(&frame, self.clock.now());
            }
        }
    }

    async fn apply(&self, st: &mut State, fx: Effects) {
        if fx.stop_playback {
            st.playback.stop(self.clock.now());
        }
        if fx.barge_in {
            Stats::inc(&self.stats.barge_ins);
        }
        if fx.turn_completed {
            Stats::inc(&self.stats.turns_completed);
        }
        let turn_cut = fx.turn_cut;
        if let Some(cmd) = fx.send
            && !self.send_control(&cmd).await
        {
            let was_waiting = matches!(st.turns.phase(), UserPhase::AwaitTurn { .. });
            st.turns.uplink_failed();
            if was_waiting {
                self.publish(st, |v| conversation::wait_ended(v, TurnOutcome::Error));
            }
            if turn_cut {
                // No `turn_end` will come for the turn we cut: close its entry here.
                self.publish(st, |v| conversation::end_open(v, TurnOutcome::Interrupted));
            }
        }
    }

    /// Must-deliver while connected: wait for queue space up to `write_timeout`.
    async fn send_control(&self, cmd: &ClientCmd) -> bool {
        let msg = match cmd {
            ClientCmd::Cancel => ClientMsg::Cancel,
            ClientCmd::End => ClientMsg::End,
            ClientCmd::Send(text) => ClientMsg::Send {
                text: Cow::Borrowed(text),
            },
        };
        let Ok(text) = encode_client(&msg) else {
            Stats::inc(&self.stats.uplink_control_dropped);
            return false;
        };
        match deadline(&*self.clock, self.cfg.write_timeout, self.out_tx.reserve()).await {
            Some(Ok(permit)) => {
                permit.send(Frame::Text(text));
                Stats::hwm(
                    &self.stats.out_queue_hwm,
                    self.out_tx.max_capacity() - self.out_tx.capacity(),
                );
                true
            }
            Some(Err(_)) | None => {
                Stats::inc(&self.stats.uplink_control_dropped);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]
    use super::*;
    use crate::{capture::CaptureGate, queues, queues::ByteBudget};
    use fogar_ports::ipc::CaptureGesture;
    use fogar_proto::server::Capabilities;
    use fogar_testkit::{FakeMic, FakeSpeaker, TestClock};
    use std::convert::Infallible;
    use std::time::Duration;
    use tokio::sync::{mpsc, watch};
    use tokio::task::JoinSet;

    // ---- property: the gateway alone never opens the microphone (spec S2) ----------------

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            // xorshift64*
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
            &xs[(self.next() % xs.len() as u64) as usize]
        }
    }

    fn random_server_json(r: &mut Rng) -> String {
        let id = *r.pick(&["t-1", "t-2", "", "x"]);
        let seq = r.next() % 4;
        let text = *r.pick(&["", "pon la serie", "hola", "  "]);
        match r.next() % 17 {
            14 => r#"{"type":"transcription_partial","text":"#.into(),
            15 => r#"{"type":"status","service":"transcriber","state":"degraded"}"#.into(),
            16 => r#"not json at all {{{"#.into(),
            0 => format!(r#"{{"type":"transcription_partial","text":"{text}"}}"#),
            1 => format!(r#"{{"type":"transcription","text":"{text}"}}"#),
            2 => {
                let origin = r.pick(&["push", "user", "other"]);
                format!(
                    r#"{{"type":"turn_start","turn_id":"{id}","turn_seq":{seq},"origin":"{origin}"}}"#
                )
            }
            3 => format!(r#"{{"type":"token","turn_id":"{id}","text":"{text}"}}"#),
            4 => {
                let reason = r.pick(&["done", "cancelled", "interrupted", "replaced", "error"]);
                format!(r#"{{"type":"turn_end","turn_id":"{id}","reason":"{reason}"}}"#)
            }
            5 | 6 => {
                let service = r.pick(&["transcriber", "tts", "orchestrator"]);
                let state = r.pick(&["degraded", "restored", "unavailable", "reconnecting"]);
                let code = r.pick(&["", r#","code":"phrase_interrupted""#, r#","code":"x""#]);
                format!(r#"{{"type":"status","service":"{service}","state":"{state}"{code}}}"#)
            }
            7 => format!(
                r#"{{"type":"error","code":"TURN_ERROR","message":"orchestrator_unavailable","fatal":false,"turn_id":"{id}"}}"#
            ),
            8 => format!(r#"{{"type":"interrupted","turn_id":"{id}"}}"#),
            9 => {
                format!(r#"{{"type":"turn_progress","turn_id":"{id}","phase":"tool","tool":"x"}}"#)
            }
            10 => r#"{"type":"ready","live_capabilities":{"transcriber":true,"tts":true}}"#.into(),
            11 => r#"{"type":"capture_start"}"#.into(),
            12 => r#"{"type":"start_recording","open_mic":true}"#.into(),
            _ => format!(r#"{{"type":"pipeline_event","stage":"stt","turn_id":"{id}"}}"#),
        }
    }

    /// The engine wired to fakes, driven directly (no link, no IPC adapter).
    struct Harness {
        mic: FakeMic,
        speaker: FakeSpeaker,
        stats: Arc<Stats>,
        conv: watch::Receiver<ConversationView>,
        in_tx: mpsc::Sender<Inbound>,
        cmd_tx: mpsc::Sender<Command>,
        out_rx: mpsc::Receiver<Frame>,
        budget: ByteBudget,
        stop: CancellationToken,
        tasks: JoinSet<()>,
    }

    fn harness(mic: FakeMic, cfg: Config) -> Harness {
        harness_with(mic, FakeSpeaker::healthy(), cfg)
    }

    fn harness_with(mic: FakeMic, speaker: FakeSpeaker, cfg: Config) -> Harness {
        let clock = Arc::new(TestClock::new());
        let cfg = Arc::new(cfg);
        let stats = Arc::new(Stats::default());
        let (out_tx, out_rx) = mpsc::channel(queues::OUTBOUND);
        let (in_tx, in_rx) = mpsc::channel(queues::INBOUND);
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(queues::ENGINE_COMMANDS);
        let (conv_tx, conv) = watch::channel(ConversationView::default());
        let engine = Engine {
            clock,
            cfg: cfg.clone(),
            stats: stats.clone(),
            out_tx,
            in_rx,
            cmd_rx,
            conv: conv_tx,
        };
        let capture = CaptureGate::new(Arc::new(mic.clone()), &cfg, stats.clone());
        let playback = PlaybackGate::new(Arc::new(speaker.clone()), stats.clone());
        let stop = CancellationToken::new();
        let mut tasks = JoinSet::new();
        tasks.spawn(engine.run(capture, playback, stop.clone()));
        Harness {
            mic,
            speaker,
            stats,
            conv,
            in_tx,
            cmd_tx,
            out_rx,
            budget: ByteBudget::new(1 << 20),
            stop,
            tasks,
        }
    }

    impl Harness {
        async fn server(&self, json: &str) {
            if let Ok(msg) = fogar_proto::decode_server(json) {
                let charge = self.budget.try_charge(1).unwrap();
                let _ = self.in_tx.send(Inbound::Server(msg, charge)).await;
            }
        }
        async fn up(&self) {
            let caps = Capabilities {
                transcriber: true,
                tts: true,
                ..Capabilities::default()
            };
            self.in_tx.send(Inbound::SessionUp(caps)).await.unwrap();
        }
        /// The person's take, as only the IPC module could ask for it.
        async fn press(&self) -> (oneshot::Sender<Infallible>, Result<(), CaptureRefusal>) {
            let (owner_tx, owner) = oneshot::channel();
            let (reply, answer) = oneshot::channel();
            let start = Start::UserAct(crate::ipc::UserAct::for_test(CaptureGesture::Toggle));
            self.cmd_tx
                .send(Command::CaptureStart {
                    start,
                    owner,
                    reply,
                })
                .await
                .unwrap();
            let got = tokio::time::timeout(Duration::from_secs(30), answer)
                .await
                .unwrap()
                .unwrap();
            (owner_tx, got)
        }
        async fn release(&self, then: CaptureThen) {
            let (tx, rx) = oneshot::channel();
            self.cmd_tx
                .send(Command::CaptureEnd(then, tx))
                .await
                .unwrap();
            rx.await.unwrap();
        }
        fn drain(&mut self) {
            while self.out_rx.try_recv().is_ok() {}
        }
        /// One TTS frame for `turn_seq`, as the link would hand it over.
        async fn audio(&self, turn_seq: u16, pcm: &[i16]) {
            let mut b = Vec::new();
            fogar_proto::audio::encode_tts_frame(turn_seq, pcm, &mut b);
            let charge = self.budget.try_charge(1).unwrap();
            self.in_tx.send(Inbound::Audio(b, charge)).await.unwrap();
        }
        async fn turn_start(&self, id: &str, seq: u16, origin: &str) {
            self.server(&format!(
                r#"{{"type":"turn_start","turn_id":"{id}","turn_seq":{seq},"origin":"{origin}"}}"#
            ))
            .await;
        }
        async fn turn_end(&self, id: &str) {
            self.server(&format!(
                r#"{{"type":"turn_end","turn_id":"{id}","reason":"done"}}"#
            ))
            .await;
        }
        /// The app says it shows `id` (as only the IPC module would ask).
        async fn visible(&self, id: &str) {
            let (tx, rx) = oneshot::channel();
            self.cmd_tx
                .send(Command::TurnVisible(id.into(), tx))
                .await
                .unwrap();
            rx.await.unwrap();
        }
        async fn cancel(&self) {
            let (tx, rx) = oneshot::channel();
            self.cmd_tx.send(Command::Cancel(tx)).await.unwrap();
            rx.await.unwrap();
        }
        fn audio_of(&self, id: &str) -> Option<fogar_ports::ipc::TurnAudio> {
            self.conv
                .borrow()
                .turns
                .iter()
                .rev()
                .find(|t| t.turn_id == id)
                .map(|t| t.audio)
        }
        async fn stop(mut self) {
            self.stop.cancel();
            while self.tasks.join_next().await.is_some() {}
        }
    }

    /// «Cualquier secuencia aleatoria de mensajes del gateway (ServerMsg, tramas de audio,
    /// cierres, reconexiones), sin capture_start, deja FakeMic::opens() == 0»: after one or more
    /// legitimate takes (whose traffic includes partials and silence warnings), no random mix of
    /// gateway traffic and IPC commands other than `CaptureStart` (texts and cancels that really
    /// run, capture ends, permission requests) opens the microphone again.
    #[tokio::test(start_paused = true)]
    async fn no_gateway_sequence_opens_the_microphone() {
        for seed in 1..=64u64 {
            let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mic = if seed % 4 == 3 {
                FakeMic::not_determined()
            } else {
                FakeMic::healthy()
            };
            let mut h = harness(mic, Config::default());
            h.up().await;
            // Legitimate takes first (none for a not-determined mic: refused).
            for _ in 0..(1 + seed % 3) {
                let (owner, answer) = h.press().await;
                if answer.is_ok() {
                    h.mic.push_all(fogar_testkit::tone_chunks(5, 0.3));
                    h.server(r#"{"type":"transcription_partial","text":"pon"}"#)
                        .await;
                    h.server(r#"{"type":"status","service":"transcriber","state":"degraded"}"#)
                        .await;
                    for _ in 0..8 {
                        h.server(&random_server_json(&mut r)).await;
                    }
                    h.release(*r.pick(&[CaptureThen::Send, CaptureThen::Review]))
                        .await;
                }
                drop(owner);
                tokio::time::sleep(Duration::from_millis(50)).await;
                h.drain();
            }
            let baseline = h.mic.opens();
            assert_eq!(baseline == 0, seed % 4 == 3, "seed {seed}");
            let mut texts = Vec::new();
            let mut cancels = Vec::new();
            let mut ends = Vec::new();
            let mut permission_asks = 0u32;
            for _ in 0..60 {
                match r.next() % 10 {
                    0 => {
                        let caps = Capabilities {
                            transcriber: r.next().is_multiple_of(2),
                            tts: r.next().is_multiple_of(2),
                            ..Capabilities::default()
                        };
                        let _ = h.in_tx.send(Inbound::SessionUp(caps)).await;
                    }
                    1 => {
                        let _ = h.in_tx.send(Inbound::SessionDown).await;
                    }
                    2 => {
                        let mut b = vec![0xA1, 0, (r.next() % 4) as u8];
                        b.extend((0..(r.next() % 64)).map(|_| r.next() as u8));
                        let charge = h.budget.try_charge(1).unwrap();
                        let _ = h.in_tx.send(Inbound::Audio(b, charge)).await;
                    }
                    3 => match r.next() % 4 {
                        0 => {
                            let (tx, rx) = oneshot::channel();
                            if h.cmd_tx
                                .try_send(Command::SendText("hola".into(), tx))
                                .is_ok()
                            {
                                texts.push(rx);
                            }
                        }
                        1 => {
                            let (tx, rx) = oneshot::channel();
                            if h.cmd_tx.try_send(Command::Cancel(tx)).is_ok() {
                                cancels.push(rx);
                            }
                        }
                        2 => {
                            let then = *r.pick(&[
                                CaptureThen::Send,
                                CaptureThen::Review,
                                CaptureThen::Discard,
                            ]);
                            let (tx, rx) = oneshot::channel();
                            if h.cmd_tx.try_send(Command::CaptureEnd(then, tx)).is_ok() {
                                ends.push(rx);
                            }
                        }
                        _ => {
                            let (tx, rx) = oneshot::channel();
                            h.cmd_tx
                                .send(Command::RequestMicPermission(tx))
                                .await
                                .unwrap();
                            permission_asks += 1;
                            // Answered before the next one, so none coalesce.
                            tokio::time::timeout(Duration::from_secs(5), rx)
                                .await
                                .unwrap()
                                .unwrap();
                        }
                    },
                    _ => h.server(&random_server_json(&mut r)).await,
                }
                if r.next().is_multiple_of(4) {
                    tokio::time::sleep(Duration::from_millis(r.next() % 20_000)).await;
                }
                h.drain();
                tokio::task::yield_now().await;
                assert_eq!(h.mic.opens(), baseline, "seed {seed}");
            }
            tokio::time::sleep(Duration::from_secs(400)).await;
            assert_eq!(h.mic.opens(), baseline, "seed {seed}");
            assert_eq!(h.mic.permission_requests(), permission_asks, "seed {seed}");
            assert_ne!(
                h.conv.borrow().activity,
                UserActivity::Recording,
                "seed {seed}"
            );
            assert_eq!(
                h.stats.snapshot().captures_opened,
                u64::from(baseline),
                "seed {seed}"
            );
            // The commands really ran (their replies were kept, so none was skipped).
            for rx in texts {
                assert!(rx.await.is_ok(), "seed {seed}: text answered");
            }
            for rx in cancels {
                assert!(rx.await.is_ok(), "seed {seed}: cancel answered");
            }
            for rx in ends {
                assert!(rx.await.is_ok(), "seed {seed}: end answered");
            }
            h.stop().await;
        }
    }

    /// Review M3: a gateway that keeps the inbound queue full must not starve the take. Real
    /// (not paused) time: a busy runtime never auto-advances a paused clock.
    #[tokio::test(flavor = "current_thread")]
    async fn a_flooding_gateway_cannot_starve_the_safety_cut() {
        let cfg = Config {
            capture_safety_cut: Duration::from_millis(200),
            ..Config::default()
        };
        let mut h = harness(FakeMic::healthy(), cfg);
        h.up().await;
        let (_owner, answer) = h.press().await;
        assert_eq!(answer, Ok(()));
        let in_tx = h.in_tx.clone();
        let budget = h.budget.clone();
        let flood = async move {
            let msg = r#"{"type":"status","service":"tts","state":"restored"}"#;
            loop {
                let m = fogar_proto::decode_server(msg).unwrap();
                let Some(charge) = budget.try_charge(1) else {
                    tokio::task::yield_now().await;
                    continue;
                };
                if in_tx.send(Inbound::Server(m, charge)).await.is_err() {
                    break;
                }
            }
        };
        let mic = h.mic.clone();
        let stats = h.stats.clone();
        let watch = async move {
            while mic.is_open() || stats.snapshot().captures_safety_cut == 0 {
                tokio::task::yield_now().await;
            }
        };
        tokio::select! {
            () = flood => panic!("flood stopped"),
            r = tokio::time::timeout(Duration::from_secs(5), watch) => {
                assert!(r.is_ok(), "the safety cut never came under the flood");
            }
        }
        assert_eq!(h.stats.snapshot().captures_safety_cut, 1);
        h.drain();
        h.stop().await;
    }

    // ---- the output suite (spec S2 «Punto único de salida de audio», S2 3c) -------------

    use fogar_ports::ipc::TurnAudio;

    /// Lets the engine handle what was sent (paused time only advances when all are idle).
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    fn pcm(base: i16, n: usize) -> Vec<i16> {
        (0..n).map(|i| base.wrapping_add(i as i16)).collect()
    }

    /// «Ninguna secuencia de mensajes del gateway sin turn_visible hace sonar FakeSpeaker»: random
    /// gateway traffic with VALID audio frames for the turns it starts, link drops, texts and
    /// cancels never open the output. Then, as a control that the rig can sound, a confirmed
    /// turn does.
    #[tokio::test(start_paused = true)]
    async fn no_gateway_sequence_sounds_without_turn_visible() {
        for seed in 1..=48u64 {
            let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut h = harness(FakeMic::healthy(), Config::default());
            h.up().await;
            for _ in 0..80 {
                match r.next() % 8 {
                    0 => {
                        let _ = h.in_tx.send(Inbound::SessionDown).await;
                        h.up().await;
                    }
                    1..=3 => {
                        let seq = (r.next() % 4) as u16;
                        let n = 1 + (r.next() % 2000) as usize;
                        h.audio(seq, &pcm(7, n)).await;
                    }
                    4 => {
                        let (tx, _rx) = oneshot::channel();
                        let _ = h.cmd_tx.try_send(Command::SendText("hola".into(), tx));
                    }
                    5 => {
                        let (tx, _rx) = oneshot::channel();
                        let _ = h.cmd_tx.try_send(Command::Cancel(tx));
                    }
                    _ => h.server(&random_server_json(&mut r)).await,
                }
                if r.next().is_multiple_of(5) {
                    tokio::time::sleep(Duration::from_millis(r.next() % 3_000)).await;
                }
                h.drain();
                settle().await;
                let sp = h.speaker.snapshot();
                assert_eq!((sp.opens, sp.accepted), (0, 0), "seed {seed}");
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
            let s = h.stats.snapshot();
            assert_eq!(
                (s.tts_frames_played, s.playback_opens),
                (0, 0),
                "seed {seed}"
            );
            assert_eq!(h.speaker.snapshot().opens, 0, "seed {seed}");
            // Control: the same rig sounds once the app confirms a turn.
            h.turn_start("ok", 9, "user").await;
            h.visible("ok").await;
            h.audio(9, &pcm(1, 480)).await;
            settle().await;
            assert_eq!(h.speaker.snapshot().accepted, 480, "seed {seed}");
            h.stop().await;
        }
    }

    /// A confirmed turn plays the audio that waited for the confirmation, in order, then the
    /// rest directly; the view says it played.
    #[tokio::test(start_paused = true)]
    async fn confirmed_turn_plays_what_waited_in_order() {
        let h = harness(FakeMic::healthy(), Config::default());
        h.up().await;
        h.turn_start("t-1", 1, "user").await;
        settle().await;
        assert_eq!(h.conv.borrow().awaiting_visible.as_deref(), Some("t-1"));
        h.audio(1, &pcm(100, 480)).await;
        h.audio(1, &pcm(2000, 480)).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            h.speaker.snapshot().opens,
            0,
            "nothing opened before the confirmation"
        );
        h.visible("t-1").await;
        settle().await;
        h.audio(1, &pcm(5000, 240)).await;
        settle().await;
        let want: Vec<i16> = [pcm(100, 480), pcm(2000, 480), pcm(5000, 240)].concat();
        assert_eq!(h.speaker.played_samples(), want);
        let s = h.stats.snapshot();
        assert_eq!(
            (s.tts_frames_played, s.tts_turns_seen, s.tts_turns_muted),
            (3, 1, 0)
        );
        assert_eq!(h.audio_of("t-1"), Some(TurnAudio::Played));
        let v = h.conv.borrow().clone();
        assert!(v.speaking && v.awaiting_visible.is_none());
        h.stop().await;
    }

    /// «Una confirmación tardía (más de 2 s) hace que se tire el audio».
    #[tokio::test(start_paused = true)]
    async fn late_confirmation_discards_the_audio() {
        let h = harness(FakeMic::healthy(), Config::default());
        h.up().await;
        h.turn_start("t-1", 1, "user").await;
        h.audio(1, &pcm(1, 480)).await;
        tokio::time::sleep(Duration::from_millis(2100)).await;
        h.visible("t-1").await;
        h.audio(1, &pcm(1, 480)).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        let sp = h.speaker.snapshot();
        assert_eq!((sp.opens, sp.accepted), (0, 0));
        let s = h.stats.snapshot();
        assert_eq!((s.tts_turns_muted, s.tts_samples_muted), (1, 960));
        assert_eq!((s.tts_turns_seen, s.tts_frames_played), (0, 0));
        assert_eq!(h.audio_of("t-1"), Some(TurnAudio::Muted));
        assert_eq!(h.conv.borrow().awaiting_visible, None);
        h.stop().await;
    }

    /// The 2 s run from the `turn_start`, not from the first audio: a confirmation at 1.9 s
    /// still plays.
    #[tokio::test(start_paused = true)]
    async fn confirmation_just_in_time_plays() {
        let h = harness(FakeMic::healthy(), Config::default());
        h.up().await;
        h.turn_start("t-1", 1, "user").await;
        tokio::time::sleep(Duration::from_millis(1900)).await;
        h.audio(1, &pcm(1, 480)).await;
        h.visible("t-1").await;
        settle().await;
        assert_eq!(h.speaker.snapshot().accepted, 480);
        h.stop().await;
    }

    /// «Un push sin la ventana a la vista no suena»: the app never confirms it.
    #[tokio::test(start_paused = true)]
    async fn push_without_the_window_on_screen_never_sounds() {
        let h = harness(FakeMic::healthy(), Config::default());
        h.up().await;
        h.turn_start("p-1", 2, "push").await;
        for _ in 0..20 {
            h.audio(2, &pcm(1, 480)).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        h.turn_end("p-1").await;
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(h.speaker.snapshot().opens, 0);
        assert_eq!(h.stats.snapshot().tts_samples_muted, 20 * 480);
        assert_eq!(h.audio_of("p-1"), Some(TurnAudio::Muted));
        h.stop().await;
    }

    /// More than 3 s of audio before the confirmation: the turn is muted at once.
    #[tokio::test(start_paused = true)]
    async fn a_full_unseen_buffer_mutes_the_turn() {
        let h = harness(FakeMic::healthy(), Config::default());
        h.up().await;
        h.turn_start("t-1", 1, "user").await;
        for _ in 0..151 {
            h.audio(1, &pcm(1, 480)).await;
        }
        settle().await;
        assert_eq!(h.audio_of("t-1"), Some(TurnAudio::Muted));
        h.visible("t-1").await;
        h.audio(1, &pcm(1, 480)).await;
        settle().await;
        assert_eq!(h.speaker.snapshot().opens, 0);
        let s = h.stats.snapshot();
        assert_eq!((s.tts_turns_muted, s.tts_samples_muted), (1, 152 * 480));
        h.stop().await;
    }

    /// A confirmation for another turn (or for nothing) unlocks nothing.
    #[tokio::test(start_paused = true)]
    async fn a_confirmation_for_another_turn_unlocks_nothing() {
        let h = harness(FakeMic::healthy(), Config::default());
        h.up().await;
        h.turn_start("t-1", 1, "user").await;
        h.visible("t-0").await;
        h.visible("").await;
        h.audio(1, &pcm(1, 480)).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(h.speaker.snapshot().opens, 0);
        assert_eq!(h.audio_of("t-1"), Some(TurnAudio::Muted));
        h.stop().await;
    }

    /// «Cancelar en mitad de una respuesta»: silence at once, and the turn's later frames do
    /// not play.
    #[tokio::test(start_paused = true)]
    async fn cancel_mid_answer_silences_at_once() {
        let h = harness(FakeMic::healthy(), Config::default());
        h.up().await;
        h.turn_start("t-1", 1, "user").await;
        h.visible("t-1").await;
        for _ in 0..5 {
            h.audio(1, &pcm(1, 480)).await;
        }
        settle().await;
        assert_eq!(h.speaker.snapshot().queued, 5 * 480);
        h.cancel().await;
        settle().await;
        let sp = h.speaker.snapshot();
        assert_eq!(sp.queued, 0, "silence at once");
        assert!(sp.stops >= 1);
        h.audio(1, &pcm(1, 480)).await;
        settle().await;
        assert_eq!(h.speaker.snapshot().accepted, 5 * 480);
        assert!(!h.conv.borrow().speaking);
        assert_eq!(h.stats.snapshot().barge_ins, 1);
        h.stop().await;
    }

    /// Issue #25: a dead output is released and reopened (with backoff) without waiting for
    /// `SessionDown`, and audio refused by it is not counted as played.
    #[tokio::test(start_paused = true)]
    async fn a_dead_output_is_released_and_reopened() {
        let h = harness(FakeMic::healthy(), Config::default());
        h.up().await;
        h.turn_start("t-1", 1, "user").await;
        h.visible("t-1").await;
        h.audio(1, &pcm(1, 480)).await;
        settle().await;
        assert_eq!(h.speaker.snapshot().opens, 1);
        h.speaker.kill();
        h.audio(1, &pcm(1, 480)).await;
        settle().await;
        let s = h.stats.snapshot();
        assert_eq!((s.playback_dead, s.tts_frames_played), (1, 1));
        assert_eq!(
            h.speaker.snapshot().open_now,
            0,
            "the dead output was released"
        );
        // The frame waits for the reopen, after the backoff.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let sp = h.speaker.snapshot();
        assert_eq!((sp.opens, sp.open_now), (2, 1));
        assert_eq!(
            sp.accepted, 960,
            "the waiting frame played on the new output"
        );
        h.audio(1, &pcm(1, 480)).await;
        settle().await;
        assert_eq!(h.stats.snapshot().tts_frames_played, 3);
        assert_eq!(h.stats.snapshot().playback_opens, 2);
        h.stop().await;
    }

    /// Issue #25: a device that stays broken is retried with growing backoff, never in a loop.
    #[tokio::test(start_paused = true)]
    async fn a_broken_output_backs_off() {
        let h = harness(FakeMic::healthy(), Config::default());
        h.up().await;
        h.turn_start("t-1", 1, "user").await;
        h.visible("t-1").await;
        h.speaker.set_open_dead(true);
        for _ in 0..1500 {
            h.audio(1, &pcm(1, 480)).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // 30 s: opens at ~0, then backoff 0.5, 1, 2, 4, 8, 8, 8 s.
        let opens = h.speaker.snapshot().opens;
        assert!((3..=9).contains(&opens), "opens {opens}");
        let s = h.stats.snapshot();
        assert_eq!(s.tts_frames_played, 0, "nothing reached a live output");
        assert_eq!(s.playback_dead, u64::from(opens));
        assert_eq!(h.speaker.snapshot().open_now, 0);
        // The device comes back: the next frames play.
        h.speaker.set_open_dead(false);
        for _ in 0..500 {
            h.audio(1, &pcm(1, 480)).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(h.stats.snapshot().tts_frames_played > 0);
        h.stop().await;
    }

    /// Energy (spec S2 B): the output closes 2 s after it drained with no turn open.
    #[tokio::test(start_paused = true)]
    async fn the_output_closes_two_seconds_after_draining_with_no_turn() {
        let h = harness(FakeMic::healthy(), Config::default());
        h.up().await;
        h.turn_start("t-1", 1, "user").await;
        h.visible("t-1").await;
        h.audio(1, &pcm(1, 4800)).await;
        h.turn_end("t-1").await;
        settle().await;
        assert_eq!(h.speaker.snapshot().open_now, 1);
        // Still queued: never closed under the audio.
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(h.speaker.snapshot().open_now, 1);
        assert!(h.conv.borrow().speaking);
        h.speaker.drain();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!h.conv.borrow().speaking);
        assert_eq!(h.speaker.snapshot().open_now, 1, "not before 2 s");
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(h.speaker.snapshot().open_now, 0);
        assert_eq!(h.stats.snapshot().playback_idle_closes, 1);
        h.stop().await;
    }

    /// The link going down silences and releases the output (energy), and the next session's
    /// turns still need their own confirmation.
    #[tokio::test(start_paused = true)]
    async fn link_down_releases_the_output() {
        let h = harness(FakeMic::healthy(), Config::default());
        h.up().await;
        h.turn_start("t-1", 1, "user").await;
        h.visible("t-1").await;
        h.audio(1, &pcm(1, 480)).await;
        settle().await;
        h.in_tx.send(Inbound::SessionDown).await.unwrap();
        settle().await;
        assert_eq!(h.speaker.snapshot().open_now, 0);
        h.up().await;
        h.turn_start("t-1", 1, "user").await;
        h.audio(1, &pcm(1, 480)).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            h.speaker.snapshot().opens,
            1,
            "a new session's turn waits for its own"
        );
        h.stop().await;
    }
}
