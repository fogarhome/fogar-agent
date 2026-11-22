//! The engine task: IPC commands (typed text, cancel) -> turn machine -> uplink; inbound events
//! -> turn machine + conversation view -> playback. Single task, single owner of all turn and
//! conversation state (no locks). Every await is either a cancel-safe `select!` branch or
//! bounded by a deadline.
//!
//! S2 Decision 1: the microphone is never opened at start. The engine does not hold the
//! microphone at all: it owns the consent gate (`capture.rs`), the single point that can open a
//! take, and only on a `Command::CaptureStart` carrying the person's `UserAct`. Gateway events
//! can end a take (silence warnings, a dropped link) but never start one.

use crate::{
    capture::{CaptureGate, Cut, GateEvent, Owner, Readiness, Reply, Start},
    config::Config,
    conversation,
    link::Inbound,
    stats::Stats,
    time::{deadline, sleep_until_opt},
    turn::{ClientCmd, Effects, TurnMachine, UserPhase},
};
use fogar_ports::{
    audio::{ActivePlayback, AudioFormat, AudioSink, MicPermission},
    clock::Clock,
    gateway::Frame,
    ipc::{
        CaptureRefusal, CaptureThen, ConversationView, Notice, TextRefusal, TurnOutcome,
        UserActivity,
    },
};
use fogar_proto::{
    ClientMsg,
    audio::TTS_RATE_HZ,
    encode_client, encode_upload_f32le, parse_tts_frame,
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
        }
    }
}

pub(crate) struct Engine<C> {
    pub(crate) clock: Arc<C>,
    pub(crate) cfg: Arc<Config>,
    pub(crate) stats: Arc<Stats>,
    pub(crate) audio_out: Arc<dyn AudioSink>,
    pub(crate) out_tx: mpsc::Sender<Frame>,
    pub(crate) in_rx: mpsc::Receiver<Inbound>,
    pub(crate) cmd_rx: mpsc::Receiver<Command>,
    pub(crate) conv: watch::Sender<ConversationView>,
}

struct State {
    turns: TurnMachine,
    capture: CaptureGate,
    playback: Option<Box<dyn ActivePlayback>>,
    /// Reused for TTS sample conversion: no allocation per frame after warm-up.
    pcm: Vec<i16>,
}

impl State {
    fn playing(&self) -> bool {
        self.playback
            .as_ref()
            .is_some_and(|p| p.queued_samples() > 0)
    }
}

impl<C: Clock> Engine<C> {
    /// `capture` is the single consent point, the only holder of the microphone; it lives in
    /// the task's state (not in `Engine`, which `&self` methods share across awaits).
    pub(crate) async fn run(mut self, capture: CaptureGate, ingress: CancellationToken) {
        let mut st = State {
            turns: TurnMachine::new(self.cfg.turn),
            capture,
            playback: None,
            pcm: Vec::with_capacity(fogar_proto::audio::TTS_MAX_FRAME_HINT_BYTES / 2),
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
        }
        // Agent stop (`ingress`): the take closes and the device is released off the runtime.
        st.capture.shutdown(&*self.clock).await;
        if let Some(pb) = st.playback.as_mut() {
            pb.stop();
        }
        // Dropping playback closes the output.
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
                // Release the output device while offline (energy).
                st.playback = None;
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
                if st.playback.is_none() {
                    st.playback = self.audio_out.open(AudioFormat::mono(TTS_RATE_HZ)).ok();
                }
                st.pcm.clear();
                st.pcm.extend(frame.samples());
                match st.playback.as_mut() {
                    Some(pb) => {
                        let accepted = pb.write(&st.pcm);
                        Stats::add(
                            &self.stats.tts_samples_dropped,
                            (st.pcm.len() - accepted.min(st.pcm.len())) as u64,
                        );
                        Stats::inc(&self.stats.tts_frames_played);
                        if let Some(t) = st.turns.turn() {
                            let id = t.turn_id.as_str();
                            self.conv
                                .send_if_modified(|v| conversation::audio_played(v, id));
                        }
                    }
                    None => Stats::add(&self.stats.tts_samples_dropped, st.pcm.len() as u64),
                }
            }
        }
    }

    async fn apply(&self, st: &mut State, fx: Effects) {
        if fx.stop_playback
            && let Some(pb) = st.playback.as_mut()
        {
            pb.stop();
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
            audio_out: Arc::new(FakeSpeaker::healthy()),
            out_tx,
            in_rx,
            cmd_rx,
            conv: conv_tx,
        };
        let capture = CaptureGate::new(Arc::new(mic.clone()), &cfg, stats.clone());
        let stop = CancellationToken::new();
        let mut tasks = JoinSet::new();
        tasks.spawn(engine.run(capture, stop.clone()));
        Harness {
            mic,
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
}
