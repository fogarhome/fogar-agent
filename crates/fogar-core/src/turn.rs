//! Turn state machine for gateway protocol v1 (spec §9, RUST-MODEL §9). Pure: no I/O, no time
//! source (callers pass `Mono`). The engine executes the returned [`Effects`].
//!
//! Two independent halves:
//! - **user side** ([`UserPhase`]): what the local user is doing (waiting for the turn our
//!   `send` opened; S2 sub-PR 3b adds the microphone take);
//! - **server side** (`turn`): the turn the gateway is streaming (normal or push; v1 cannot tell
//!   them apart, so "push" is our heuristic: a `turn_start` we did not ask for).
//!
//! Audio gating: TTS frames are accepted only if their `turn_seq` equals the seq of the active
//! `turn_start` (compare by equality; it wraps) and was not cancelled locally (a new text or
//! the cancel button).

use fogar_ports::clock::Mono;
use fogar_proto::server::{ErrorMsg, ServerMsg, TurnOrigin};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserPhase {
    /// No gateway session: text is refused.
    Offline,
    Idle,
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
}

/// Redacting `Debug`: `Send` carries the user's transcript (review sec#7, LOG-1).
impl std::fmt::Debug for ClientCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientCmd::Send(t) => write!(f, "Send(<redacted {} bytes>)", t.len()),
            ClientCmd::Cancel => f.write_str("Cancel"),
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnTimeouts {
    /// Max wait for `turn_start` after `send` (the server's own turn timeout is 120 s).
    pub turn_start: Duration,
}

impl Default for TurnTimeouts {
    fn default() -> Self {
        Self {
            turn_start: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TurnMachine {
    phase: UserPhase,
    turn: Option<ActiveTurn>,
    /// Seq cut locally (new text or cancel): its late frames are dropped.
    suppressed_seq: Option<u16>,
    timeouts: TurnTimeouts,
}

impl TurnMachine {
    pub fn new(timeouts: TurnTimeouts) -> Self {
        Self {
            phase: UserPhase::Offline,
            turn: None,
            suppressed_seq: None,
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
    }

    /// The session ended. Turn ids restart on the next connection, so nothing carries over.
    pub fn link_down(&mut self) -> Effects {
        let had_audio = self.turn.is_some();
        self.phase = UserPhase::Offline;
        self.turn = None;
        self.suppressed_seq = None;
        Effects {
            stop_playback: had_audio,
            ..Effects::default()
        }
    }

    /// The person sent text. `playing` = playback still has queued audio. A new `send`
    /// replaces any running turn server-side (`turn_end{replaced}`), so no `cancel` is sent; the
    /// running turn's late frames are suppressed and what is queued stops at once. `None` when
    /// offline (the caller answers `NotReady`).
    pub fn send_text(&mut self, text: String, playing: bool, now: Mono) -> Option<Effects> {
        if self.phase == UserPhase::Offline {
            return None;
        }
        let mut fx = self.cut(playing);
        fx.send = Some(ClientCmd::Send(text));
        self.phase = UserPhase::AwaitTurn { since: now };
        Some(fx)
    }

    /// The cancel button: `cancel` goes out if a turn streams or our `send` still waits for its
    /// `turn_start`; playback stops either way.
    pub fn cancel(&mut self, playing: bool) -> Effects {
        let waiting = matches!(self.phase, UserPhase::AwaitTurn { .. });
        let streaming = self.turn.is_some();
        let mut fx = self.cut(playing);
        if waiting || streaming {
            fx.send = Some(ClientCmd::Cancel);
        }
        if waiting {
            self.phase = UserPhase::Idle;
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
        } else if playing {
            // The turn already ended; only its ~2 s playback tail is left.
            fx.stop_playback = true;
            fx.barge_in = true;
        }
        fx
    }

    /// The uplink could not deliver a control message: give up on the current user action.
    pub fn uplink_failed(&mut self) {
        if self.phase != UserPhase::Offline {
            self.phase = UserPhase::Idle;
        }
    }

    /// Next instant at which [`TurnMachine::on_tick`] has something to do.
    pub fn next_deadline(&self) -> Option<Mono> {
        match self.phase {
            UserPhase::AwaitTurn { since } => Some(since.saturating_add(self.timeouts.turn_start)),
            UserPhase::Offline | UserPhase::Idle => None,
        }
    }

    /// Returns true if a user-side wait timed out (and was abandoned).
    pub fn on_tick(&mut self, now: Mono) -> bool {
        match self.next_deadline() {
            Some(d) if now >= d => {
                self.phase = UserPhase::Idle;
                true
            }
            _ => false,
        }
    }

    pub fn on_server(&mut self, msg: &ServerMsg) -> Effects {
        let mut fx = Effects::default();
        match msg {
            ServerMsg::TurnStart(ts) => {
                let awaiting = matches!(self.phase, UserPhase::AwaitTurn { .. });
                // M19 says who started the turn; older gateways do not, so infer it from
                // whether we were waiting for an answer.
                let origin = match ts.origin {
                    Some(TurnOrigin::Push) => Origin::Push,
                    Some(TurnOrigin::User) => Origin::User,
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
            ServerMsg::Ready(_)
            | ServerMsg::Transcription(_)
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
        // A TURN_ERROR before turn_start means our send failed (e.g. orchestrator_unavailable):
        // stop waiting. Once a turn started, its turn_end still follows (spec §7).
        if e.is_turn_error()
            && self.turn.is_none()
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
                m.on_server(&ts("t-1", 7, Some(TurnOrigin::Push)));
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
        let fx = m.on_server(&ts("t-1", 1, None));
        assert!(fx.turn_started);
        assert_eq!(m.turn().map(|t| t.origin), Some(Origin::User), "we waited");
        assert_eq!(m.phase(), UserPhase::Idle);
        assert!(m.accept_audio(1) && !m.accept_audio(2));
        assert!(m.on_server(&te("t-1", None)).turn_completed);
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
                m.on_server(&ts("t-1", 3, Some(TurnOrigin::Push)));
            }
            let fx = m.cancel(playing);
            let want = sends.then_some(ClientCmd::Cancel);
            assert_eq!(fx.send, want, "{name}");
            assert_eq!(fx.stop_playback, stop, "{name}");
            assert_eq!(m.phase(), UserPhase::Idle, "{name}");
            assert_eq!(m.next_deadline(), None, "{name}: no wait left");
            assert!(!m.accept_audio(3), "{name}");
        }
        let mut off = TurnMachine::new(TurnTimeouts::default());
        assert_eq!(off.cancel(false), Effects::default());
    }

    #[test]
    fn cancelled_turn_end_is_not_ours_and_interrupted_stops() {
        let mut m = online();
        m.on_server(&ts("t-1", 7, None));
        assert_eq!(m.turn().map(|t| t.origin), Some(Origin::Push));
        m.cancel(true);
        let end = te("t-1", Some(TurnEndReason::Cancelled));
        assert!(!m.on_server(&end).turn_completed);
        let i = ServerMsg::Interrupted(Interrupted {
            turn_id: Some("t-1".into()),
        });
        assert!(m.on_server(&i).stop_playback);
    }

    /// Gateway M19: no `interrupted` message; the turn_end carries the reason.
    #[test]
    fn m19_turn_end_reason_stops_playback_and_origin_is_trusted() {
        let mut m = online();
        // Waiting for our answer, but the server says this turn is a push: trust it.
        m.send_text("hola".into(), false, T0);
        m.on_server(&ts("t-1", 1, Some(TurnOrigin::Push)));
        assert_eq!(m.turn().map(|t| t.origin), Some(Origin::Push));
        assert!(matches!(m.phase(), UserPhase::AwaitTurn { .. }));
        let fx = m.on_server(&te("t-1", Some(TurnEndReason::Interrupted)));
        assert!(fx.turn_completed && fx.stop_playback);
        m.on_server(&ts("t-2", 2, Some(TurnOrigin::User)));
        assert_eq!(m.turn().map(|t| t.origin), Some(Origin::User));
        let replaced = te("t-2", Some(TurnEndReason::Replaced));
        assert!(m.on_server(&replaced).stop_playback);
        m.on_server(&ts("t-3", 3, Some(TurnOrigin::User)));
        let fx = m.on_server(&te("t-3", Some(TurnEndReason::Done)));
        assert!(
            fx.turn_completed && !fx.stop_playback,
            "a finished turn plays to the end"
        );
        // `closed` precedes a 4102/4200 close: the tail must not play on a dead session.
        m.on_server(&ts("t-4", 4, Some(TurnOrigin::User)));
        let closed = te("t-4", Some(TurnEndReason::Closed));
        assert!(m.on_server(&closed).stop_playback);
    }

    #[test]
    fn waits_time_out_and_errors_release() {
        let mut m = online();
        m.send_text("x".into(), false, T0);
        let d = m.next_deadline().unwrap();
        assert!(!m.on_tick(Mono(Duration::from_secs(1))));
        assert!(m.on_tick(d));
        assert_eq!(m.phase(), UserPhase::Idle);
        m.send_text("x".into(), false, T0);
        let err = ServerMsg::Error(ErrorMsg {
            code: "TURN_ERROR".into(),
            message: "orchestrator_unavailable".into(),
            fatal: false,
            turn_id: None,
        });
        m.on_server(&err);
        assert_eq!(m.phase(), UserPhase::Idle);
        m.send_text("x".into(), false, T0);
        m.uplink_failed();
        assert_eq!(m.phase(), UserPhase::Idle);
    }

    #[test]
    fn link_down_resets_everything() {
        let mut m = online();
        m.on_server(&ts("t-1", 1, None));
        assert!(m.link_down().stop_playback);
        assert_eq!(m.phase(), UserPhase::Offline);
        assert!(!m.accept_audio(1));
    }
}
