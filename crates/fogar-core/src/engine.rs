//! The engine task: IPC commands (typed text, cancel, takes) and link events -> conversation
//! view; capture and playback gates. Single task, single owner of all turn and conversation
//! state (no locks). Every await is either a cancel-safe `select!` branch or bounded by a
//! deadline.
//!
//! S4 PR 4a, Task 3 (transport swap): the link is `GET /events` and there is no uplink. Until
//! the conversation store and its REST calls land (Task 4) the engine follows the link's
//! up/down only:
//! - shortcut: `/events` events and `resync` are dropped here, so the conversation view stays
//!   empty; Task 4 feeds the store.
//! - shortcut: `SendText` is refused `NotReady` and `CancelTurn` does nothing (there is no turn
//!   to cancel); Task 4 posts them over HTTP.
//! - Dictation is off until PR 5: no link reports a transcriber, so the consent gate refuses
//!   every take `NoTranscriber` (the gate's own rule, not a special case here).
//! - Nothing plays: no path feeds the playback gate.
//!
//! S2 Decision 1: the microphone is never opened at start. The engine does not hold the
//! microphone at all: it owns the consent gate (`capture.rs`), the single point that can open a
//! take, and only on a `Command::CaptureStart` carrying the person's `UserAct`. Gateway events
//! can end a take (a dropped link) but never start one.
//!
//! S2 3c: the engine does not hold the speaker either. Output goes through the playback gate
//! (`playback.rs`), the single output point: a turn's audio plays only once the app confirmed
//! the turn is on screen (`Command::TurnVisible`).

use crate::{
    capture::{CaptureGate, Cut, GateEvent, Owner, Readiness, Reply, Start},
    config::Config,
    conversation,
    link::Inbound,
    playback::PlaybackGate,
    stats::Stats,
    time::sleep_until_opt,
    turn::{Effects, UserPhase},
};
use fogar_ports::{
    audio::MicPermission,
    clock::Clock,
    ipc::{
        CaptureRefusal, CaptureThen, ConversationView, Notice, TextRefusal, TurnOutcome,
        UserActivity,
    },
};
use std::sync::Arc;
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
    /// The app shows this turn (its id as the view carries it): its audio may play. Answered
    /// `false` when the id names more than one entry of the view (ambiguous: refused).
    TurnVisible(String, oneshot::Sender<bool>),
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
    pub(crate) in_rx: mpsc::Receiver<Inbound>,
    pub(crate) cmd_rx: mpsc::Receiver<Command>,
    pub(crate) conv: watch::Sender<ConversationView>,
}

struct State {
    turns: crate::turn::TurnMachine,
    capture: CaptureGate,
    playback: PlaybackGate,
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
            turns: crate::turn::TurnMachine::new(self.cfg.turn),
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
                // never starve the take's cuts (safety, silence, owner gone) or its device jobs.
                ev = st.capture.next(&*self.clock) => self.on_capture(&mut st, ev),
                // The output's own timers and device jobs are not starved by a busy gateway.
                () = st.playback.next(&*self.clock) => {}
                ev = self.in_rx.recv() => match ev {
                    None => break,
                    Some(ev) => self.on_inbound(&mut st, ev),
                },
                cmd = self.cmd_rx.recv(), if cmds_open => match cmd {
                    None => cmds_open = false,
                    Some(cmd) => self.on_command(&mut st, cmd),
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
                        self.apply(&mut st, fx);
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

    /// The playback gate follows the turn machine's turn, and the view follows the gate. No turn
    /// starts in 4a, so this stays idle; kept so PR 5 only has to feed the turns again.
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
    /// machine). The view lives inside the `watch`: no copy per change.
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

    fn on_command(&self, st: &mut State, cmd: Command) {
        match cmd {
            Command::SendText(_text, reply) => {
                // The IPC gave up (and counted it as busy): never act on it late.
                if reply.is_closed() {
                    return;
                }
                // Also while the device is opening (review B4): the take is already the person's.
                if st.turns.phase() == UserPhase::Recording || st.capture.is_active() {
                    let _ = reply.send(Err(TextRefusal::Recording));
                    return;
                }
                // shortcut: no sending until the conversation store posts it over HTTP (S4 PR 4a
                // Task 4).
                let _ = reply.send(Err(TextRefusal::NotReady));
            }
            Command::Cancel(reply) => {
                // shortcut: there is no turn to cancel until Task 4 (`POST …/cancel`).
                let _ = reply.send(());
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
                self.end_take(st, then, None);
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
                // Review F2 of #32: the confirmation must name ONE entry of the view without
                // ambiguity (ids are compared as the view shows them, truncated included).
                let entries = self
                    .conv
                    .borrow()
                    .turns
                    .iter()
                    .filter(|t| !id.is_empty() && t.turn_id == id)
                    .count();
                if entries > 1 {
                    Stats::inc(&self.stats.turn_visible_refused);
                    let _ = reply.send(false);
                    return;
                }
                let _ = reply.send(true);
                st.playback.visible(&id, self.clock.now());
            }
        }
    }

    fn on_capture(&self, st: &mut State, ev: GateEvent) {
        match ev {
            GateEvent::Nothing => {}
            GateEvent::Refused { reply, why } => {
                let _ = reply.send(Err(why));
                self.refresh_mic(st);
            }
            GateEvent::Opened { reply } => {
                // Unreachable while no link reports a transcriber (the gate refuses first);
                // kept so a take that did open is closed, never left on.
                let now = self.clock.now();
                st.capture.close(now, CaptureRefusal::NotReady);
                let _ = reply.send(Err(CaptureRefusal::NoTranscriber));
                self.refresh_mic(st);
            }
            GateEvent::Chunk(_) => {
                // No uplink in 4a: nothing to stream to.
                Stats::inc(&self.stats.mic_chunks);
            }
            GateEvent::Cut(cut) => {
                match cut {
                    Cut::Silence => Stats::inc(&self.stats.captures_silence_cut),
                    Cut::Safety => Stats::inc(&self.stats.captures_safety_cut),
                    Cut::OwnerGone | Cut::DeviceEnded => {}
                }
                self.end_take(st, cut.then(), cut.notice());
            }
            GateEvent::Permission(p, waiters) => {
                for w in waiters {
                    let _ = w.send(p);
                }
                self.refresh_mic(st);
            }
        }
    }

    /// Closes the take (if any): the device is released off the runtime. A take shorter than
    /// `capture_min` is discarded whatever was asked.
    fn end_take(&self, st: &mut State, then: CaptureThen, notice: Option<Notice>) {
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
        self.apply(st, fx);
    }

    fn on_inbound(&self, st: &mut State, ev: Inbound) {
        match ev {
            Inbound::Up { .. } => {
                st.turns.link_up();
                self.refresh_mic(st);
                self.publish(st, |_| false);
            }
            Inbound::Down | Inbound::SignedOut => {
                // The link that heard the take is gone: close it.
                let took = st.capture.close(self.clock.now(), CaptureRefusal::NotReady);
                let fx = st.turns.link_down();
                self.publish(st, |v| {
                    let t = took.is_some() && conversation::take_ended(v, None);
                    conversation::link_down(v) | t
                });
                self.apply(st, fx);
                // Silence, and release the output device while offline (energy).
                st.playback.link_down(self.clock.now());
            }
            // shortcut: the conversation store consumes these in S4 PR 4a Task 4; dropping the
            // event releases its byte charge.
            Inbound::Event { .. } | Inbound::Resync => {}
        }
    }

    /// The turn machine's effects. There is no uplink in 4a: anything it wanted to send fails
    /// (counted), exactly like a missed uplink deadline did.
    fn apply(&self, st: &mut State, fx: Effects) {
        if fx.stop_playback {
            st.playback.stop(self.clock.now());
        }
        if fx.turn_completed {
            Stats::inc(&self.stats.turns_completed);
        }
        if fx.send.is_some() {
            Stats::inc(&self.stats.uplink_control_dropped);
            let was_waiting = matches!(st.turns.phase(), UserPhase::AwaitTurn { .. });
            st.turns.uplink_failed();
            if was_waiting {
                self.publish(st, |v| conversation::wait_ended(v, TurnOutcome::Error));
            }
            if fx.turn_cut {
                self.publish(st, |v| conversation::end_open(v, TurnOutcome::Interrupted));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]
    use super::*;
    use crate::{
        capture::CaptureGate,
        link::Api,
        queues::{self, ByteBudget},
    };
    use fogar_ports::{gateway::GatewayAddr, ipc::CaptureGesture};
    use fogar_proto::events::decode_event;
    use fogar_testkit::{FakeMic, FakeSpeaker, TestClock};
    use std::convert::Infallible;
    use std::time::Duration;
    use tokio::task::JoinSet;

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

    /// Random `/events` data, valid and not.
    fn random_event_json(r: &mut Rng) -> String {
        let id = *r.pick(&["t-1", "t-2", "", "x"]);
        let text = *r.pick(&["", "pon la serie", "hola", "  "]);
        let conv = *r.pick(&["c-1", "c-2", ""]);
        match r.next() % 10 {
            0 => format!(
                r#"{{"type":"message","id":"m","conversation":"{conv}","text":"{text}","state":"delivered","from":{{"source":"device","self":false}}}}"#
            ),
            1 => format!(
                r#"{{"type":"turn_start","turn_id":"{id}","conversation":"{conv}","origin":"push"}}"#
            ),
            2 => format!(
                r#"{{"type":"token","turn_id":"{id}","conversation":"{conv}","text":"{text}"}}"#
            ),
            3 => format!(r#"{{"type":"turn_end","turn_id":"{id}","reason":"done"}}"#),
            4 => r#"{"type":"audio","turn_id":"t-1","data":"AAAA"}"#.into(),
            5 => r#"{"type":"error","code":"x","message":"y"}"#.into(),
            6 => r#"{"type":"ready","protocol":5}"#.into(),
            7 => r#"{"type":"start_recording","open_mic":true}"#.into(),
            8 => r#"not json at all {{{"#.into(),
            _ => r#"{"type":"resync"}"#.into(),
        }
    }

    /// The engine wired to fakes, driven directly (no link, no IPC adapter).
    struct Harness {
        mic: FakeMic,
        speaker: FakeSpeaker,
        conv: watch::Receiver<ConversationView>,
        in_tx: mpsc::Sender<Inbound>,
        cmd_tx: mpsc::Sender<Command>,
        budget: ByteBudget,
        stop: CancellationToken,
        tasks: JoinSet<()>,
    }

    fn harness(mic: FakeMic) -> Harness {
        let clock = Arc::new(TestClock::new());
        let cfg = Arc::new(Config::default());
        let stats = Arc::new(Stats::default());
        let (in_tx, in_rx) = mpsc::channel(queues::INBOUND);
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(queues::ENGINE_COMMANDS);
        let (conv_tx, conv) = watch::channel(ConversationView::default());
        let engine = Engine {
            clock,
            cfg: cfg.clone(),
            stats: stats.clone(),
            in_rx,
            cmd_rx,
            conv: conv_tx,
        };
        let speaker = FakeSpeaker::healthy();
        let capture = CaptureGate::new(Arc::new(mic.clone()), &cfg, stats.clone());
        let playback = PlaybackGate::new(Arc::new(speaker.clone()), stats);
        let stop = CancellationToken::new();
        let mut tasks = JoinSet::new();
        tasks.spawn(engine.run(capture, playback, stop.clone()));
        Harness {
            mic,
            speaker,
            conv,
            in_tx,
            cmd_tx,
            budget: ByteBudget::new(1 << 20),
            stop,
            tasks,
        }
    }

    impl Harness {
        async fn up(&self) {
            let api = Api {
                addr: GatewayAddr::parse("127.0.0.1:8004").unwrap(),
                token: "t".to_owned().into(),
            };
            let up = Inbound::Up {
                api,
                resumed: false,
            };
            self.in_tx.send(up).await.unwrap();
        }
        async fn event(&self, json: &str) {
            let charge = self.budget.try_charge(json.len()).unwrap();
            let ev = Inbound::Event {
                ev: decode_event(json),
                id: Some(1),
                charge,
            };
            let _ = self.in_tx.send(ev).await;
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
        async fn send_text(&self) -> Result<(), TextRefusal> {
            let (tx, rx) = oneshot::channel();
            self.cmd_tx
                .send(Command::SendText("hola".into(), tx))
                .await
                .unwrap();
            rx.await.unwrap()
        }
        async fn stop(mut self) {
            self.stop.cancel();
            while self.tasks.join_next().await.is_some() {}
        }
    }

    /// Dictation is off until PR 5: with the link up, a take is refused (no transcriber) and the
    /// microphone never opens; a typed text is refused `NotReady` until Task 4.
    #[tokio::test(start_paused = true)]
    async fn dictation_and_sending_are_refused_in_4a() {
        let h = harness(FakeMic::healthy());
        h.up().await;
        let (_owner, answer) = h.press().await;
        assert_eq!(answer, Err(CaptureRefusal::NoTranscriber));
        assert_eq!(h.send_text().await, Err(TextRefusal::NotReady));
        assert_eq!(h.mic.opens(), 0);
        h.stop().await;
    }

    /// «Cualquier secuencia aleatoria de mensajes del gateway … deja FakeMic::opens() == 0»,
    /// over the `/events` vocabulary: random events, link ups and downs, re-fetches, sign-outs,
    /// texts, cancels, capture ends and permission requests never open the microphone nor
    /// sound the speaker, and the view stays empty.
    #[tokio::test(start_paused = true)]
    async fn no_gateway_sequence_opens_the_microphone_or_sounds() {
        for seed in 1..=48u64 {
            let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let h = harness(FakeMic::healthy());
            h.up().await;
            for _ in 0..60 {
                match r.next() % 10 {
                    0 => h.up().await,
                    1 => {
                        let _ = h.in_tx.send(Inbound::Down).await;
                    }
                    2 => {
                        let _ = h.in_tx.send(Inbound::Resync).await;
                    }
                    3 => {
                        let _ = h.in_tx.send(Inbound::SignedOut).await;
                    }
                    4 => match r.next() % 4 {
                        0 => {
                            let (tx, _rx) = oneshot::channel();
                            let _ = h.cmd_tx.try_send(Command::SendText("hola".into(), tx));
                        }
                        1 => {
                            let (tx, _rx) = oneshot::channel();
                            let _ = h.cmd_tx.try_send(Command::Cancel(tx));
                        }
                        2 => {
                            let (tx, _rx) = oneshot::channel();
                            let _ = h
                                .cmd_tx
                                .try_send(Command::CaptureEnd(CaptureThen::Send, tx));
                        }
                        _ => {
                            let (tx, rx) = oneshot::channel();
                            h.cmd_tx
                                .send(Command::RequestMicPermission(tx))
                                .await
                                .unwrap();
                            let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
                        }
                    },
                    _ => h.event(&random_event_json(&mut r)).await,
                }
                if r.next().is_multiple_of(4) {
                    tokio::time::sleep(Duration::from_millis(r.next() % 20_000)).await;
                }
                tokio::task::yield_now().await;
                assert_eq!(h.mic.opens(), 0, "seed {seed}");
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
            assert_eq!(h.mic.opens(), 0, "seed {seed}");
            assert_eq!(h.speaker.snapshot().opens, 0, "seed {seed}");
            assert!(h.conv.borrow().turns.is_empty(), "seed {seed}");
            assert_eq!(h.budget.in_use(), 0, "seed {seed}: every charge released");
            h.stop().await;
        }
    }
}
