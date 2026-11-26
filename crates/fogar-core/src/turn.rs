//! Turn state machine for gateway protocol v1 (spec §9, RUST-MODEL §9). Pure: no I/O, no time
//! source (callers pass `Mono`). The engine executes the returned [`Effects`].
//!
//! Two independent halves:
//! - **user side** ([`UserPhase`]): what the local user is doing (recording a take, waiting for
//!   its final transcript, waiting for the turn our `send` opened);
//! - **server side** (`turn`): the turn the gateway is streaming (normal or push; v1 cannot tell
//!   them apart, so "push" is our heuristic: a `turn_start` we did not ask for).
//!
//! Audio gating: TTS frames are accepted only if their `turn_seq` equals the seq of the active
//! `turn_start` (compare by equality; it wraps) and was not cancelled locally (a new text or
//! the cancel button).

use fogar_ports::{
    clock::Mono,
    ipc::{CaptureThen, UserText},
};
use fogar_proto::server::{ErrorMsg, ServerMsg, TurnOrigin};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserPhase {
    /// No gateway session: text is refused.
    Offline,
    Idle,
    /// A take is open (the capture gate holds the microphone): its audio streams to the gateway.
    Recording,
    /// Sent `end`; waiting for the final `transcription`, then `then` decides what happens to it.
    AwaitTranscription {
        since: Mono,
        then: CaptureThen,
    },
    /// Sent `send`; waiting for its `turn_start`.
    AwaitTurn {
        since: Mono,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Opened by our `send`.
    User,
    /// Not requested by us (agent push, or a turn we can no longer correlate).
    Push,
}

/// The turn being played. The reply text is not kept: nothing shows it yet (re-add, bounded and
/// with a redacting `Debug`, when a UI does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveTurn {
    pub turn_id: String,
    pub turn_seq: u16,
    pub origin: Origin,
}

/// Client messages the engine must send.
#[derive(Clone, PartialEq, Eq)]
pub enum ClientCmd {
    Send(String),
    Cancel,
    /// End of the take's audio (a final `transcription` follows).
    End,
}

/// Redacting `Debug`: `Send` carries the user's transcript (review sec#7, LOG-1).
impl std::fmt::Debug for ClientCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientCmd::Send(t) => write!(f, "Send(<redacted {} bytes>)", t.len()),
            ClientCmd::Cancel => f.write_str("Cancel"),
            ClientCmd::End => f.write_str("End"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Effects {
    pub send: Option<ClientCmd>,
    /// Discard queued playback now (new text, cancel, interrupted).
    pub stop_playback: bool,
    pub turn_started: bool,
    pub turn_completed: bool,
    pub barge_in: bool,
    /// A streaming turn was cut locally (new text or cancel). If the control message then fails
    /// to go out, no `turn_end` will close it: the engine ends its view entry itself.
    pub turn_cut: bool,
    /// The final transcript of a take ended with `Review`: it goes to the text box, never sent.
    pub review: Option<UserText>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnTimeouts {
    /// Max wait for `turn_start` after `send` (the server's own turn timeout is 120 s).
    pub turn_start: Duration,
    /// Max wait for the final `transcription` after `end` (GUESSED: the gateway answers every
    /// `end` with exactly one, usually within a second).
    pub transcription: Duration,
}

/// How long an abandoned `send` may still produce its `turn_start` (the gateway's own turn
/// timeout, 120 s). GUESSED: no later `turn_start` belongs to it.
pub const ABANDONED_SEND_TTL: Duration = Duration::from_secs(120);

impl Default for TurnTimeouts {
    fn default() -> Self {
        Self {
            turn_start: Duration::from_secs(30),
            transcription: Duration::from_secs(15),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TurnMachine {
    phase: UserPhase,
    turn: Option<ActiveTurn>,
    /// Seq cut locally (new text or cancel): its late frames are dropped.
    suppressed_seq: Option<u16>,
    /// `send`s we stopped waiting for (cancel, local timeout, a newer text) whose `turn_start`
    /// may still arrive. Sends are never matched to their turn otherwise, so such a late
    /// `turn_start` would play in full and show as a phantom turn. CERTAIN (3.0.1 §5): while the
    /// session is open every accepted `send` gets exactly one `turn_start`/`turn_end` pair, even
    /// if cancelled or replaced before it starts. GUESSED: at most `ABANDONED_SEND_TTL` late.
    /// Saturating.
    abandoned: u8,
    /// When `abandoned` was last bumped; the count expires `ABANDONED_SEND_TTL` after it.
    abandoned_at: Mono,
    /// Final transcripts still owed by the gateway for takes we no longer wait on (a new take,
    /// a text or the cancel button while transcribing): skipped when they arrive. Saturating;
    /// reset with the session and when a transcription wait times out.
    finals_to_skip: u8,
    timeouts: TurnTimeouts,
}

impl TurnMachine {
    pub fn new(timeouts: TurnTimeouts) -> Self {
        Self {
            phase: UserPhase::Offline,
            turn: None,
            suppressed_seq: None,
            abandoned: 0,
            abandoned_at: Mono::ZERO,
            finals_to_skip: 0,
            timeouts,
        }
    }

    pub fn phase(&self) -> UserPhase {
        self.phase
    }

    pub fn turn(&self) -> Option<&ActiveTurn> {
        self.turn.as_ref()
    }

    /// A gateway session reached `ready`.
    pub fn link_up(&mut self) {
        self.phase = UserPhase::Idle;
        self.turn = None;
        self.suppressed_seq = None;
        self.abandoned = 0;
        self.finals_to_skip = 0;
    }

    /// The session ended. Turn ids restart on the next connection, so nothing carries over.
    pub fn link_down(&mut self) -> Effects {
        let had_audio = self.turn.is_some();
        self.phase = UserPhase::Offline;
        self.turn = None;
        self.suppressed_seq = None;
        self.abandoned = 0;
        self.finals_to_skip = 0;
        Effects {
            stop_playback: had_audio,
            ..Effects::default()
        }
    }

    /// The person sent text. `playing` = playback still has queued audio. A new `send`
    /// replaces any running turn server-side (`turn_end{replaced}`), so no `cancel` is sent; the
    /// running turn's late frames are suppressed and what is queued stops at once. `None` when
    /// offline or while a take is open (the caller answers `NotReady` / `Recording`).
    pub fn send_text(&mut self, text: String, playing: bool, now: Mono) -> Option<Effects> {
        match self.phase {
            UserPhase::Offline | UserPhase::Recording => return None,
            // The earlier send's turn_start may still come (then replaced by this one).
            UserPhase::AwaitTurn { .. } => self.abandon(now),
            // The take's final transcript still comes: it must not be sent or reviewed now.
            UserPhase::AwaitTranscription { .. } => self.skip_final(),
            UserPhase::Idle => {}
        }
        let mut fx = self.cut(playing);
        fx.send = Some(ClientCmd::Send(text));
        self.phase = UserPhase::AwaitTurn { since: now };
        Some(fx)
    }

    fn skip_final(&mut self) {
        self.finals_to_skip = self.finals_to_skip.saturating_add(1);
    }

    /// A take opened (the capture gate already holds the microphone; S2 «toma»). Like the cancel
    /// button for what plays: playback stops, and `cancel` goes out if a turn streams or our
    /// `send` still waits. A previous take's final transcript still owed is skipped. `None` when
    /// offline or already recording (the caller closes the take).
    pub fn utterance_start(&mut self, playing: bool, now: Mono) -> Option<Effects> {
        match self.phase {
            UserPhase::Offline | UserPhase::Recording => return None,
            UserPhase::AwaitTranscription { .. } => self.skip_final(),
            UserPhase::Idle | UserPhase::AwaitTurn { .. } => {}
        }
        let fx = self.cancel(playing, now);
        self.phase = UserPhase::Recording;
        Some(fx)
    }

    /// The take ended: `end` goes out and `then` decides what happens to the final transcript.
    /// Nothing when no take was recording (offline: the session that heard it is gone).
    pub fn utterance_end(&mut self, then: CaptureThen, now: Mono) -> Effects {
        if self.phase != UserPhase::Recording {
            return Effects::default();
        }
        self.phase = UserPhase::AwaitTranscription { since: now, then };
        Effects {
            send: Some(ClientCmd::End),
            ..Effects::default()
        }
    }

    fn abandon(&mut self, now: Mono) {
        self.abandoned = self.abandoned.saturating_add(1);
        self.abandoned_at = now;
    }

    /// The cancel button: `cancel` goes out if a turn streams or our `send` still waits for its
    /// `turn_start`; playback stops either way.
    pub fn cancel(&mut self, playing: bool, now: Mono) -> Effects {
        let waiting = matches!(self.phase, UserPhase::AwaitTurn { .. });
        let streaming = self.turn.is_some();
        let mut fx = self.cut(playing);
        if waiting || streaming {
            fx.send = Some(ClientCmd::Cancel);
        }
        if waiting {
            self.phase = UserPhase::Idle;
            self.abandon(now);
        }
        if let UserPhase::AwaitTranscription { .. } = self.phase {
            // The person cancelled: the transcript must not turn into a `send` afterwards.
            self.phase = UserPhase::Idle;
            self.skip_final();
        }
        fx
    }

    /// Drops the turn being played (its late frames are suppressed) and stops playback.
    fn cut(&mut self, playing: bool) -> Effects {
        let mut fx = Effects::default();
        if let Some(t) = self.turn.take() {
            self.suppressed_seq = Some(t.turn_seq);
            fx.stop_playback = true;
            fx.barge_in = true;
            fx.turn_cut = true;
        } else if playing {
            // The turn already ended; only its ~2 s playback tail is left.
            fx.stop_playback = true;
            fx.barge_in = true;
        }
        fx
    }

    /// The uplink could not deliver a control message: give up on the current user action.
    pub fn uplink_failed(&mut self) {
        match self.phase {
            // The take itself goes on: its audio is best effort anyway.
            UserPhase::Offline | UserPhase::Recording => {}
            UserPhase::Idle
            | UserPhase::AwaitTurn { .. }
            | UserPhase::AwaitTranscription { .. } => {
                self.phase = UserPhase::Idle;
            }
        }
    }

    /// Next instant at which [`TurnMachine::on_tick`] has something to do.
    pub fn next_deadline(&self) -> Option<Mono> {
        match self.phase {
            UserPhase::AwaitTurn { since } => Some(since.saturating_add(self.timeouts.turn_start)),
            UserPhase::AwaitTranscription { since, .. } => {
                Some(since.saturating_add(self.timeouts.transcription))
            }
            UserPhase::Offline | UserPhase::Idle | UserPhase::Recording => None,
        }
    }

    /// `Some` if the user-side wait timed out: a turn wait is abandoned and `cancel` goes out so
    /// the gateway (whose own limit is 120 s) does not answer it later; a transcription wait just
    /// ends (nothing to cancel; a late final is then ignored as long as no new take waits).
    pub fn on_tick(&mut self, now: Mono) -> Option<Effects> {
        match self.next_deadline() {
            Some(d) if now >= d && matches!(self.phase, UserPhase::AwaitTranscription { .. }) => {
                self.phase = UserPhase::Idle;
                self.finals_to_skip = 0;
                Some(Effects::default())
            }
            Some(d) if now >= d => {
                self.phase = UserPhase::Idle;
                self.abandon(now);
                Some(Effects {
                    send: Some(ClientCmd::Cancel),
                    ..Effects::default()
                })
            }
            _ => None,
        }
    }

    pub fn on_server(&mut self, msg: &ServerMsg, now: Mono) -> Effects {
        let mut fx = Effects::default();
        if self.abandoned > 0 && now >= self.abandoned_at.saturating_add(ABANDONED_SEND_TTL) {
            self.abandoned = 0;
        }
        match msg {
            ServerMsg::TurnStart(ts)
                if self.abandoned > 0 && ts.origin != Some(TurnOrigin::Push) =>
            {
                // The late answer to a send we abandoned: never played, never shown, and a live
                // wait for a newer send stays open. Its turn_end matches nothing.
                self.abandoned -= 1;
                self.suppressed_seq = Some(ts.turn_seq);
            }
            ServerMsg::TurnStart(ts) => {
                let awaiting = matches!(self.phase, UserPhase::AwaitTurn { .. });
                // M19 says who started the turn; older gateways do not, so infer it from
                // whether we were waiting for an answer. Review F3 of #32: a `user` turn with no
                // send of ours waiting is not "mine" (nothing here asked for it): a push.
                let origin = match ts.origin {
                    Some(TurnOrigin::Push) => Origin::Push,
                    Some(TurnOrigin::User) if awaiting => Origin::User,
                    Some(TurnOrigin::User) => Origin::Push,
                    None if awaiting => Origin::User,
                    None => Origin::Push,
                };
                if awaiting && origin == Origin::User {
                    self.phase = UserPhase::Idle;
                }
                if self.suppressed_seq == Some(ts.turn_seq) {
                    self.suppressed_seq = None;
                }
                // A new turn_start replaces the previous turn (the server sent its turn_end, or
                // will never send one: JGW-286).
                self.turn = Some(ActiveTurn {
                    turn_id: ts.turn_id.clone(),
                    turn_seq: ts.turn_seq,
                    origin,
                });
                fx.turn_started = true;
            }
            ServerMsg::TurnEnd(te) => {
                if self.turn.as_ref().is_some_and(|t| t.turn_id == te.turn_id) {
                    self.turn = None;
                    fx.turn_completed = true;
                    // M19 gateway-side barge-in (the old `interrupted` message) or a `send`
                    // that replaced the turn: what is still buffered must not play on.
                    if te.reason.is_some_and(|r| r.stops_playback()) {
                        fx.stop_playback = true;
                    }
                }
            }
            ServerMsg::Interrupted(_) => {
                // Gateway <= 2.x barge-in; arrives after the cancelled turn's turn_end.
                self.turn = None;
                fx.stop_playback = true;
            }
            ServerMsg::Error(e) => self.on_error(e),
            ServerMsg::Transcription(_) if self.finals_to_skip > 0 => self.finals_to_skip -= 1,
            ServerMsg::Transcription(t) => {
                if let UserPhase::AwaitTranscription { then, .. } = self.phase {
                    self.phase = UserPhase::Idle;
                    let text = t.text.trim();
                    match then {
                        _ if text.is_empty() => {}
                        CaptureThen::Send => {
                            // Like a typed text: it replaces whatever turn streams now.
                            fx = self.cut(false);
                            fx.send = Some(ClientCmd::Send(text.to_owned()));
                            self.phase = UserPhase::AwaitTurn { since: now };
                        }
                        CaptureThen::Review => fx.review = Some(UserText(text.to_owned())),
                        // Discard, and any outcome a newer contract adds: never sent on its own.
                        _ => {}
                    }
                }
            }
            ServerMsg::Ready(_)
            | ServerMsg::TranscriptionPartial(_)
            | ServerMsg::ToolCall(_)
            | ServerMsg::Status(_)
            | ServerMsg::Token(_)
            | ServerMsg::PipelineEvent(_)
            | ServerMsg::TurnProgress(_)
            | ServerMsg::Unknown => {}
        }
        fx
    }

    fn on_error(&mut self, e: &ErrorMsg) {
        // A TURN_ERROR that names no turn we are playing means our send failed (e.g.
        // orchestrator_unavailable): stop waiting. One naming the playing turn is about that
        // turn, whose turn_end still follows (spec §7). Same rule as `conversation::on_server`.
        let names_playing = e.turn_id.as_deref().is_some_and(|id| {
            !id.is_empty() && self.turn.as_ref().is_some_and(|t| t.turn_id == id)
        });
        if e.is_turn_error()
            && !names_playing
            && let UserPhase::AwaitTurn { .. } = self.phase
        {
            self.phase = UserPhase::Idle;
        }
    }

    /// Whether a TTS frame with this `turn_seq` may be played.
    pub fn accept_audio(&self, turn_seq: u16) -> bool {
        self.suppressed_seq != Some(turn_seq)
            && self.turn.as_ref().is_some_and(|t| t.turn_seq == turn_seq)
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::ClientCmd;

    #[test]
    fn client_cmd_debug_redacts_the_transcript() {
        let d = format!("{:?}", ClientCmd::Send("my PIN is 4321".into()));
        assert!(!d.contains("4321") && d.contains("redacted"), "{d}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fogar_proto::server::{Interrupted, TurnEnd, TurnEndReason, TurnStart};

    fn ts(id: &str, seq: u16, origin: Option<TurnOrigin>) -> ServerMsg {
        ServerMsg::TurnStart(TurnStart {
            turn_id: id.into(),
            turn_seq: seq,
            origin,
            conversation: None,
        })
    }
    fn te(id: &str, reason: Option<TurnEndReason>) -> ServerMsg {
        ServerMsg::TurnEnd(TurnEnd {
            turn_id: id.into(),
            reason,
        })
    }
    const T0: Mono = Mono::ZERO;
    fn online() -> TurnMachine {
        let mut m = TurnMachine::new(TurnTimeouts::default());
        m.link_up();
        m
    }

    #[test]
    fn send_text_table() {
        // (name, a turn streams, playback queued) -> playback stops
        for (name, streaming, playing, stop) in [
            ("idle", false, false, false),
            ("tail playing", false, true, true),
            ("turn streaming", true, true, true),
        ] {
            let mut m = online();
            if streaming {
                m.on_server(&ts("t-1", 7, Some(TurnOrigin::Push)), T0);
            }
            let fx = m.send_text("hola".into(), playing, T0).expect(name);
            assert_eq!(
                fx.send,
                Some(ClientCmd::Send("hola".into())),
                "{name}: a send, never a cancel"
            );
            assert_eq!((fx.stop_playback, fx.barge_in), (stop, stop), "{name}");
            assert_eq!(m.phase(), UserPhase::AwaitTurn { since: T0 }, "{name}");
            assert!(
                m.turn().is_none() && !m.accept_audio(7),
                "{name}: old frames dropped"
            );
        }
        let mut off = TurnMachine::new(TurnTimeouts::default());
        assert_eq!(off.send_text("hola".into(), true, T0), None, "offline");
    }

    #[test]
    fn my_text_opens_a_user_turn() {
        let mut m = online();
        m.send_text("pon la serie".into(), false, T0);
        let fx = m.on_server(&ts("t-1", 1, None), T0);
        assert!(fx.turn_started);
        assert_eq!(m.turn().map(|t| t.origin), Some(Origin::User), "we waited");
        assert_eq!(m.phase(), UserPhase::Idle);
        assert!(m.accept_audio(1) && !m.accept_audio(2));
        assert!(m.on_server(&te("t-1", None), T0).turn_completed);
        assert!(!m.accept_audio(1));
    }

    #[test]
    fn cancel_table() {
        // (name, waiting for turn_start, a turn streams, playback queued) -> (sends, stops)
        for (name, waiting, streaming, playing, sends, stop) in [
            ("nothing", false, false, false, false, false),
            ("tail only", false, false, true, false, true),
            ("waiting", true, false, false, true, false),
            ("streaming", false, true, true, true, true),
        ] {
            let mut m = online();
            if waiting {
                m.send_text("x".into(), false, T0);
            }
            if streaming {
                m.on_server(&ts("t-1", 3, Some(TurnOrigin::Push)), T0);
            }
            let fx = m.cancel(playing, T0);
            let want = sends.then_some(ClientCmd::Cancel);
            assert_eq!(fx.send, want, "{name}");
            assert_eq!(fx.stop_playback, stop, "{name}");
            assert_eq!(m.phase(), UserPhase::Idle, "{name}");
            assert_eq!(m.next_deadline(), None, "{name}: no wait left");
            assert!(!m.accept_audio(3), "{name}");
        }
        let mut off = TurnMachine::new(TurnTimeouts::default());
        assert_eq!(off.cancel(false, T0), Effects::default());
    }

    #[test]
    fn cancelled_turn_end_is_not_ours_and_interrupted_stops() {
        let mut m = online();
        m.on_server(&ts("t-1", 7, None), T0);
        assert_eq!(m.turn().map(|t| t.origin), Some(Origin::Push));
        m.cancel(true, T0);
        let end = te("t-1", Some(TurnEndReason::Cancelled));
        assert!(!m.on_server(&end, T0).turn_completed);
        let i = ServerMsg::Interrupted(Interrupted {
            turn_id: Some("t-1".into()),
        });
        assert!(m.on_server(&i, T0).stop_playback);
    }

    /// Gateway M19: no `interrupted` message; the turn_end carries the reason.
    #[test]
    fn m19_turn_end_reason_stops_playback_and_origin_is_trusted() {
        let mut m = online();
        // Waiting for our answer, but the server says this turn is a push: trust it.
        m.send_text("hola".into(), false, T0);
        m.on_server(&ts("t-1", 1, Some(TurnOrigin::Push)), T0);
        assert_eq!(m.turn().map(|t| t.origin), Some(Origin::Push));
        assert!(matches!(m.phase(), UserPhase::AwaitTurn { .. }));
        let fx = m.on_server(&te("t-1", Some(TurnEndReason::Interrupted)), T0);
        assert!(fx.turn_completed && fx.stop_playback);
        // Still waiting for "hola": this `user` turn is its answer.
        m.on_server(&ts("t-2", 2, Some(TurnOrigin::User)), T0);
        assert_eq!(m.turn().map(|t| t.origin), Some(Origin::User));
        let replaced = te("t-2", Some(TurnEndReason::Replaced));
        assert!(m.on_server(&replaced, T0).stop_playback);
        m.on_server(&ts("t-3", 3, Some(TurnOrigin::User)), T0);
        let fx = m.on_server(&te("t-3", Some(TurnEndReason::Done)), T0);
        assert!(
            fx.turn_completed && !fx.stop_playback,
            "a finished turn plays to the end"
        );
        // `closed` precedes a 4011/4020/4200 close: the tail must not play on a dead session.
        m.on_server(&ts("t-4", 4, Some(TurnOrigin::User)), T0);
        let closed = te("t-4", Some(TurnEndReason::Closed));
        assert!(m.on_server(&closed, T0).stop_playback);
    }

    /// Review F3 of #32: a `user` turn with no send of ours waiting is not "mine": a push.
    #[test]
    fn an_unsolicited_user_turn_is_a_push() {
        let mut m = online();
        m.on_server(&ts("t-1", 1, Some(TurnOrigin::User)), T0);
        assert_eq!(m.turn().map(|t| t.origin), Some(Origin::Push));
        m.send_text("hola".into(), false, T0);
        m.on_server(&ts("t-2", 2, Some(TurnOrigin::User)), T0);
        assert_eq!(m.turn().map(|t| t.origin), Some(Origin::User));
    }

    #[test]
    fn waits_time_out_and_errors_release() {
        let mut m = online();
        m.send_text("x".into(), false, T0);
        let d = m.next_deadline().unwrap();
        assert!(m.on_tick(Mono(Duration::from_secs(1))).is_none());
        assert_eq!(m.on_tick(d).and_then(|fx| fx.send), Some(ClientCmd::Cancel));
        assert_eq!(m.phase(), UserPhase::Idle);
        m.send_text("x".into(), false, T0);
        let err = ServerMsg::Error(ErrorMsg {
            code: "TURN_ERROR".into(),
            message: "orchestrator_unavailable".into(),
            fatal: false,
            turn_id: None,
        });
        m.on_server(&err, T0);
        assert_eq!(m.phase(), UserPhase::Idle);
        m.send_text("x".into(), false, T0);
        m.uplink_failed();
        assert_eq!(m.phase(), UserPhase::Idle);
    }

    #[test]
    fn link_down_resets_everything() {
        let mut m = online();
        m.on_server(&ts("t-1", 1, None), T0);
        assert!(m.link_down().stop_playback);
        assert_eq!(m.phase(), UserPhase::Offline);
        assert!(!m.accept_audio(1));
    }

    fn at(s: u64) -> Mono {
        Mono(Duration::from_secs(s))
    }

    /// Review M1 (a): cancel while waiting, then the gateway's racing turn_start arrives.
    #[test]
    fn late_turn_start_after_cancel_is_ignored() {
        for origin in [None, Some(TurnOrigin::User)] {
            let mut m = online();
            m.send_text("x".into(), false, T0);
            assert_eq!(m.cancel(false, T0).send, Some(ClientCmd::Cancel));
            let fx = m.on_server(&ts("t-1", 1, origin), at(1));
            assert!(!fx.turn_started, "{origin:?}");
            assert!(
                m.turn().is_none() && !m.accept_audio(1),
                "{origin:?}: never played"
            );
            assert!(
                !m.on_server(&te("t-1", Some(TurnEndReason::Cancelled)), at(1))
                    .turn_completed
            );
            // Consumed: the next answer is ours again.
            m.send_text("y".into(), false, at(2));
            assert!(m.on_server(&ts("t-2", 2, origin), at(2)).turn_started);
            assert!(m.accept_audio(2));
        }
    }

    /// Review M1 (b): local timeout sends `cancel`; a turn_start after it is not played.
    #[test]
    fn late_turn_start_after_timeout_is_ignored_and_expires() {
        let mut m = online();
        m.send_text("x".into(), false, T0);
        let d = m.next_deadline().unwrap();
        assert_eq!(m.on_tick(d).and_then(|fx| fx.send), Some(ClientCmd::Cancel));
        assert!(!m.on_server(&ts("t-1", 1, None), at(60)).turn_started);
        assert!(!m.accept_audio(1));
        // An abandoned send expires after ABANDONED_SEND_TTL: a later push-less turn plays.
        let mut m = online();
        m.send_text("x".into(), false, T0);
        m.on_tick(m.next_deadline().unwrap());
        let late = d.saturating_add(ABANDONED_SEND_TTL);
        assert!(m.on_server(&ts("t-1", 1, None), late).turn_started);
        // Pushes are never taken for a late answer.
        let mut m = online();
        m.send_text("x".into(), false, T0);
        m.cancel(false, T0);
        assert!(
            m.on_server(&ts("t-1", 1, Some(TurnOrigin::Push)), at(1))
                .turn_started
        );
    }

    /// Review M1 (c): two quick texts; t-1 answers the first and must not end the second's wait.
    #[test]
    fn two_quick_texts_pair_with_their_own_turns() {
        let mut m = online();
        m.send_text("uno".into(), false, T0);
        m.send_text("dos".into(), false, T0);
        let fx = m.on_server(&ts("t-1", 1, Some(TurnOrigin::User)), T0);
        assert!(!fx.turn_started && !m.accept_audio(1));
        assert!(
            matches!(m.phase(), UserPhase::AwaitTurn { .. }),
            "still waiting for dos"
        );
        assert!(
            m.on_server(&ts("t-2", 2, Some(TurnOrigin::User)), T0)
                .turn_started
        );
        assert_eq!(m.phase(), UserPhase::Idle);
        assert!(m.accept_audio(2));
    }

    #[test]
    fn abandoned_count_is_cleared_by_the_link() {
        let mut m = online();
        m.send_text("x".into(), false, T0);
        m.cancel(false, T0);
        m.link_down();
        m.link_up();
        m.send_text("y".into(), false, T0);
        assert!(m.on_server(&ts("t-1", 1, None), T0).turn_started);
    }

    /// Review L4: a TURN_ERROR naming a turn we never saw start still ends the wait; one naming
    /// the playing turn does not.
    #[test]
    fn turn_error_rule() {
        let err = |id: Option<&str>| {
            ServerMsg::Error(ErrorMsg {
                code: "TURN_ERROR".into(),
                message: "orchestrator_error".into(),
                fatal: false,
                turn_id: id.map(Into::into),
            })
        };
        let mut m = online();
        m.on_server(&ts("t-1", 1, Some(TurnOrigin::Push)), T0);
        m.send_text("x".into(), false, T0);
        m.on_server(&ts("t-2", 2, Some(TurnOrigin::Push)), T0);
        m.on_server(&err(Some("t-2")), T0);
        assert!(
            matches!(m.phase(), UserPhase::AwaitTurn { .. }),
            "about the playing push"
        );
        m.on_server(&err(Some("t-9")), T0);
        assert_eq!(
            m.phase(),
            UserPhase::Idle,
            "names no turn we play: our send failed"
        );
        m.send_text("y".into(), false, T0);
        m.on_server(&err(None), T0);
        assert_eq!(
            m.phase(),
            UserPhase::Idle,
            "no id, even with a turn playing"
        );
    }
}
