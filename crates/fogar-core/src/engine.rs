//! The engine task: IPC commands (typed text, cancel) -> turn machine -> uplink; inbound events
//! -> turn machine + conversation view -> playback. Single task, single owner of all turn and
//! conversation state (no locks). Every await is either a cancel-safe `select!` branch or
//! bounded by a deadline.
//!
//! S2 Decision 1: the microphone is never opened at start; there is no capture path at all until
//! the consent gate (`capture.rs`, sub-PR 3b). The engine only reads the permission STATUS
//! (`mic_permission`, which never prompts) for the view.

use crate::{
    config::Config,
    conversation,
    link::Inbound,
    stats::Stats,
    time::{deadline, sleep_until_opt},
    turn::{ClientCmd, Effects, TurnMachine, UserPhase},
};
use fogar_ports::{
    audio::{ActivePlayback, AudioFormat, AudioSink, AudioSource},
    clock::Clock,
    gateway::Frame,
    ipc::{ConversationView, TextRefusal, TurnOutcome, UserActivity},
};
use fogar_proto::{ClientMsg, audio::TTS_RATE_HZ, encode_client, parse_tts_frame};
use std::{borrow::Cow, sync::Arc};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

/// IPC -> engine (`queues::ENGINE_COMMANDS`). Each carries the reply channel; the IPC side waits
/// for it under `write_timeout` and drops it on expiry, and the engine then skips the command
/// (`is_closed`) instead of acting on a request the UI was told failed.
pub(crate) enum Command {
    SendText(String, oneshot::Sender<Result<(), TextRefusal>>),
    Cancel(oneshot::Sender<()>),
}

/// Redacting `Debug`: the text is the user's.
impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::SendText(t, _) => write!(f, "SendText(<redacted {} bytes>)", t.len()),
            Command::Cancel(_) => f.write_str("Cancel"),
        }
    }
}

pub(crate) struct Engine<C> {
    pub(crate) clock: Arc<C>,
    pub(crate) cfg: Arc<Config>,
    pub(crate) stats: Arc<Stats>,
    pub(crate) audio_in: Arc<dyn AudioSource>,
    pub(crate) audio_out: Arc<dyn AudioSink>,
    pub(crate) out_tx: mpsc::Sender<Frame>,
    pub(crate) in_rx: mpsc::Receiver<Inbound>,
    pub(crate) cmd_rx: mpsc::Receiver<Command>,
    pub(crate) conv: watch::Sender<ConversationView>,
}

struct State {
    turns: TurnMachine,
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
    pub(crate) async fn run(mut self, ingress: CancellationToken) {
        let mut st = State {
            turns: TurnMachine::new(self.cfg.turn),
            playback: None,
            pcm: Vec::with_capacity(fogar_proto::audio::TTS_MAX_FRAME_HINT_BYTES / 2),
        };
        self.refresh_mic();
        // No IPC (or it stopped): the closed channel must not turn the branch into a hot loop.
        let mut cmds_open = true;
        loop {
            let wake_at = st.turns.next_deadline();
            tokio::select! {
                biased;
                () = ingress.cancelled() => break,
                ev = self.in_rx.recv() => match ev {
                    None => break,
                    Some(ev) => self.on_inbound(&mut st, ev).await,
                },
                cmd = self.cmd_rx.recv(), if cmds_open => match cmd {
                    None => cmds_open = false,
                    Some(cmd) => self.on_command(&mut st, cmd).await,
                },
                () = sleep_until_opt(&*self.clock, wake_at), if wake_at.is_some() => {
                    if st.turns.on_tick(self.clock.now()) {
                        Stats::inc(&self.stats.user_waits_timed_out);
                        self.publish(&st, |v| conversation::wait_ended(v, TurnOutcome::Timeout));
                    }
                }
            }
        }
        if let Some(pb) = st.playback.as_mut() {
            pb.stop();
        }
        // Dropping playback closes the output.
    }

    /// Applies `f` to the view and publishes it if anything changed (activity follows the turn
    /// machine). The view lives inside the `watch`: no copy per token.
    fn publish(&self, st: &State, f: impl FnOnce(&mut ConversationView) -> bool) {
        let activity = match st.turns.phase() {
            UserPhase::AwaitTurn { .. } => UserActivity::AwaitingReply,
            UserPhase::Offline | UserPhase::Idle => UserActivity::Idle,
        };
        self.conv
            .send_if_modified(|v| f(v) | conversation::set_activity(v, activity));
    }

    /// The permission STATUS only (the port guarantees it never prompts).
    fn refresh_mic(&self) {
        let m = self.audio_in.mic_permission();
        self.conv.send_if_modified(|v| conversation::set_mic(v, m));
    }

    async fn on_command(&self, st: &mut State, cmd: Command) {
        match cmd {
            Command::SendText(text, reply) => {
                // The IPC gave up (and counted it as busy): never act on it late.
                if reply.is_closed() {
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
                let fx = st.turns.cancel(playing);
                if fx.send.is_some() {
                    Stats::inc(&self.stats.cancels_sent);
                }
                // A wait with no turn yet ends here; a streaming turn ends with its turn_end.
                self.publish(st, |v| conversation::wait_ended(v, TurnOutcome::Cancelled));
                self.apply(st, fx).await;
            }
        }
    }

    async fn on_inbound(&self, st: &mut State, ev: Inbound) {
        match ev {
            Inbound::SessionUp(caps) => {
                st.turns.link_up();
                self.refresh_mic();
                self.publish(st, |v| conversation::link_up(v, caps));
            }
            Inbound::SessionDown => {
                let fx = st.turns.link_down();
                self.publish(st, conversation::link_down);
                self.apply(st, fx).await;
                // Release the output device while offline (energy).
                st.playback = None;
            }
            Inbound::Server(msg, _charge) => {
                let fx = st.turns.on_server(&msg);
                if fx.turn_started {
                    Stats::inc(&self.stats.turns_started);
                }
                let origin = st
                    .turns
                    .turn()
                    .filter(|_| fx.turn_started)
                    .map(|t| t.origin);
                self.publish(st, |v| conversation::on_server(v, &msg, origin));
                self.apply(st, fx).await;
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
        if let Some(cmd) = fx.send
            && !self.send_control(&cmd).await
        {
            let was_waiting = matches!(st.turns.phase(), UserPhase::AwaitTurn { .. });
            st.turns.uplink_failed();
            if was_waiting {
                self.publish(st, |v| conversation::wait_ended(v, TurnOutcome::Error));
            }
        }
    }

    /// Must-deliver while connected: wait for queue space up to `write_timeout`.
    async fn send_control(&self, cmd: &ClientCmd) -> bool {
        let msg = match cmd {
            ClientCmd::Cancel => ClientMsg::Cancel,
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
