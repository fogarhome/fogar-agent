//! The conversation as the UI draws it (spec S2, Decision 3): a bounded
//! [`ConversationView`] owned by the engine task and published through a `watch` (latest value
//! wins, no backlog). Pure: every function mutates the view in place and returns whether it
//! changed, so the engine can drive `watch::Sender::send_if_modified` without cloning the view.
//!
//! Bounds: at most `CONVERSATION_MAX_TURNS` turns and `CONVERSATION_MAX_TEXT_BYTES` of text
//! (`mine` + `reply`), oldest turns dropped first; gateway-supplied ids and codes are cut to
//! [`ID_MAX_BYTES`]. A turn is matched only by a NON-empty id: the turn we opened with `send`
//! waits under an empty id until its `turn_start`, and must never catch a stray `token`.
//! `Debug` of the view types prints sizes, never content (fogar-ports).

use crate::turn::Origin;
use fogar_ports::{
    audio::MicPermission,
    ipc::{
        CONVERSATION_MAX_TEXT_BYTES, CONVERSATION_MAX_TURNS, ConversationView, Notice,
        ProgressKind, ServiceState, TurnAudio, TurnOutcome, TurnSource, TurnView,
    },
};
use fogar_proto::server::{
    self, Capabilities, ErrorMsg, ProgressPhase, ServerMsg, Service, Status, TurnEndReason,
};

/// Longest gateway id or code kept (`turn_id`, `conversation`, error code).
pub const ID_MAX_BYTES: usize = 128;

/// `s` cut to [`ID_MAX_BYTES`] at a char boundary.
fn bounded(s: &str) -> &str {
    if s.len() <= ID_MAX_BYTES {
        return s;
    }
    let mut n = ID_MAX_BYTES;
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// The open turn with this gateway id; never matches an empty id.
fn open_turn<'a>(v: &'a mut ConversationView, id: &str) -> Option<&'a mut TurnView> {
    if id.is_empty() {
        return None;
    }
    let id = bounded(id);
    v.turns
        .iter_mut()
        .rev()
        .find(|t| t.turn_id == id && t.outcome.is_none())
}

/// The turn our `send` opened that has no `turn_start` yet.
fn pending(v: &mut ConversationView) -> Option<&mut TurnView> {
    v.turns
        .iter_mut()
        .rev()
        .find(|t| t.turn_id.is_empty() && t.outcome.is_none() && !t.mine.is_empty())
}

fn text_bytes(v: &ConversationView) -> usize {
    v.turns.iter().map(|t| t.mine.len() + t.reply.len()).sum()
}

/// Drops the oldest turns until both bounds hold; a single turn over the text bound has its
/// reply cut (at a char boundary).
fn trim(v: &mut ConversationView) {
    let excess = v.turns.len().saturating_sub(CONVERSATION_MAX_TURNS);
    v.turns.drain(..excess);
    while v.turns.len() > 1 && text_bytes(v) > CONVERSATION_MAX_TEXT_BYTES {
        v.turns.remove(0);
    }
    if let Some(t) = v.turns.first_mut() {
        let room = CONVERSATION_MAX_TEXT_BYTES.saturating_sub(t.mine.len());
        if t.reply.len() > room {
            let mut n = room;
            while !t.reply.is_char_boundary(n) {
                n -= 1;
            }
            t.reply.truncate(n);
        }
    }
}

fn outcome(r: Option<TurnEndReason>) -> TurnOutcome {
    match r {
        None | Some(TurnEndReason::Done) => TurnOutcome::Done,
        Some(TurnEndReason::Error) => TurnOutcome::Error,
        Some(TurnEndReason::Cancelled) => TurnOutcome::Cancelled,
        Some(TurnEndReason::Interrupted) => TurnOutcome::Interrupted,
        Some(TurnEndReason::Replaced) => TurnOutcome::Replaced,
        Some(TurnEndReason::Timeout) => TurnOutcome::Timeout,
        Some(TurnEndReason::Closed) => TurnOutcome::Closed,
    }
}

fn progress(p: ProgressPhase) -> ProgressKind {
    match p {
        ProgressPhase::Preparing => ProgressKind::Preparing,
        ProgressPhase::Thinking => ProgressKind::Thinking,
        ProgressPhase::Tool => ProgressKind::Tool,
        ProgressPhase::Working => ProgressKind::Working,
    }
}

/// GUESSED: for `TURN_ERROR` the stable reason is in `message` (fogar-proto `ErrorMsg`), so a
/// known reason is kept instead of the generic code; anything else keeps `code`.
fn error_code(e: &ErrorMsg) -> String {
    let known = e
        .turn_reason()
        .is_some_and(|r| r != server::TurnErrorReason::Other);
    bounded(if known { &e.message } else { &e.code }).to_owned()
}

fn end(t: &mut TurnView, o: TurnOutcome) {
    t.outcome = Some(o);
    t.progress = None;
    t.tool = None;
}

fn caps_state(live: bool) -> ServiceState {
    if live {
        ServiceState::Up
    } else {
        ServiceState::Down
    }
}

/// A `status` state as the UI shows it; unknown states are `Unknown`.
fn status_state(s: &Status) -> ServiceState {
    match s.state() {
        server::ServiceState::Restored => ServiceState::Up,
        server::ServiceState::Degraded => ServiceState::Degraded,
        server::ServiceState::Unavailable | server::ServiceState::Reconnecting => {
            ServiceState::Down
        }
        server::ServiceState::Other => ServiceState::Unknown,
    }
}

fn set<T: PartialEq>(slot: &mut T, v: T) -> bool {
    let changed = *slot != v;
    *slot = v;
    changed
}

/// A session reached `ready`: its live capabilities are the current voice state.
pub fn link_up(v: &mut ConversationView, caps: Capabilities) -> bool {
    set(&mut v.transcriber, caps_state(caps.transcriber)) | set(&mut v.tts, caps_state(caps.tts))
}

/// The session ended: every open turn is `Closed`, the voice state is unknown again.
pub fn link_down(v: &mut ConversationView) -> bool {
    let mut changed = false;
    for t in v.turns.iter_mut().filter(|t| t.outcome.is_none()) {
        end(t, TurnOutcome::Closed);
        changed = true;
    }
    changed |= set(&mut v.transcriber, ServiceState::Unknown);
    changed | set(&mut v.tts, ServiceState::Unknown)
}

pub fn set_mic(v: &mut ConversationView, m: MicPermission) -> bool {
    set(&mut v.mic, m)
}

pub fn set_activity(v: &mut ConversationView, a: fogar_ports::ipc::UserActivity) -> bool {
    set(&mut v.activity, a)
}

/// The person's text went out with `send`. It shows at once, under an empty id until its
/// `turn_start`; an earlier text still waiting is replaced (the gateway replaces it too).
pub fn sent(v: &mut ConversationView, text: &str) -> bool {
    if let Some(t) = pending(v) {
        end(t, TurnOutcome::Replaced);
    }
    v.notice = None;
    let mut t = TurnView::default();
    t.mine = text.to_owned();
    t.source = TurnSource::Mine;
    v.turns.push(t);
    trim(v);
    true
}

/// The wait for our turn ended without a `turn_start` (timeout, cancel, uplink failure).
pub fn wait_ended(v: &mut ConversationView, o: TurnOutcome) -> bool {
    match pending(v) {
        Some(t) => {
            end(t, o);
            true
        }
        None => false,
    }
}

/// TTS audio of this turn reached the speaker.
pub fn audio_played(v: &mut ConversationView, turn_id: &str) -> bool {
    match open_turn(v, turn_id) {
        Some(t) => set(&mut t.audio, TurnAudio::Played),
        None => false,
    }
}

/// A gateway event. `origin` is the turn machine's verdict for a `turn_start` (M19 `origin`,
/// or inferred on older gateways).
pub fn on_server(v: &mut ConversationView, msg: &ServerMsg, origin: Option<Origin>) -> bool {
    match msg {
        ServerMsg::TurnStart(ts) => {
            let id = bounded(&ts.turn_id).to_owned();
            let conversation = ts.conversation.as_deref().map(|c| bounded(c).to_owned());
            let mine = origin != Some(Origin::Push);
            match pending(v).filter(|_| mine) {
                Some(t) => {
                    t.turn_id = id;
                    t.conversation = conversation;
                }
                None => {
                    let mut t = TurnView::default();
                    t.turn_id = id;
                    t.conversation = conversation;
                    t.source = if mine {
                        TurnSource::Mine
                    } else {
                        TurnSource::Push
                    };
                    v.turns.push(t);
                    trim(v);
                }
            }
            true
        }
        ServerMsg::Token(tok) => match open_turn(v, &tok.turn_id) {
            Some(t) => {
                t.reply.push_str(&tok.text);
                t.progress = None;
                t.tool = None;
                trim(v);
                true
            }
            None => false,
        },
        ServerMsg::TurnProgress(p) => match open_turn(v, &p.turn_id) {
            Some(t) => {
                t.progress = Some(progress(p.phase));
                // fogar-proto already dropped names over 128 chars or outside `[A-Za-z0-9_.:-]`.
                t.tool = p.tool.clone();
                true
            }
            None => false,
        },
        ServerMsg::TurnEnd(te) => match open_turn(v, &te.turn_id) {
            Some(t) => {
                end(t, outcome(te.reason));
                true
            }
            None => false,
        },
        ServerMsg::Error(e) => {
            let t = match e.turn_id.as_deref() {
                Some(id) if !id.is_empty() => open_turn(v, id),
                // A TURN_ERROR before `turn_start` means our `send` failed.
                _ if e.is_turn_error() => pending(v),
                _ => None,
            };
            match t {
                Some(t) => {
                    end(t, TurnOutcome::Error);
                    t.error_code = Some(error_code(e));
                    true
                }
                None => false,
            }
        }
        ServerMsg::Status(s) => {
            let state = status_state(s);
            match s.service() {
                Service::Transcriber => {
                    let lost = s.code.as_deref() == Some("phrase_interrupted")
                        && set(&mut v.notice, Some(Notice::PhraseLost));
                    set(&mut v.transcriber, state) | lost
                }
                Service::Tts => set(&mut v.tts, state),
                Service::Orchestrator | Service::Other => false,
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fogar_ports::ipc::UserActivity;
    use fogar_proto::server::{Token, TurnEnd, TurnProgress, TurnStart};

    fn start(id: &str, conv: Option<&str>) -> ServerMsg {
        ServerMsg::TurnStart(TurnStart {
            turn_id: id.into(),
            turn_seq: 1,
            origin: None,
            conversation: conv.map(Into::into),
        })
    }
    fn token(id: &str, text: &str) -> ServerMsg {
        ServerMsg::Token(Token {
            turn_id: id.into(),
            text: text.into(),
        })
    }
    fn end_msg(id: &str, r: Option<TurnEndReason>) -> ServerMsg {
        ServerMsg::TurnEnd(TurnEnd {
            turn_id: id.into(),
            reason: r,
        })
    }
    fn status(service: &str, state: &str, code: Option<&str>) -> ServerMsg {
        ServerMsg::Status(Status {
            service: service.into(),
            state: state.into(),
            code: code.map(Into::into),
            message: None,
        })
    }
    fn err(code: &str, message: &str, turn_id: Option<&str>) -> ServerMsg {
        ServerMsg::Error(ErrorMsg {
            code: code.into(),
            message: message.into(),
            fatal: false,
            turn_id: turn_id.map(Into::into),
        })
    }
    const USER: Option<Origin> = Some(Origin::User);
    const PUSH: Option<Origin> = Some(Origin::Push);

    #[test]
    fn my_text_becomes_a_turn_with_progress_reply_and_outcome() {
        let mut v = ConversationView::default();
        assert!(sent(&mut v, "pon la serie"));
        assert_eq!(v.turns.len(), 1);
        assert!(on_server(&mut v, &start("t-1", Some("c-1")), USER));
        let p = ServerMsg::TurnProgress(TurnProgress {
            turn_id: "t-1".into(),
            phase: ProgressPhase::Tool,
            tool: Some("ha_call_service".into()),
        });
        assert!(on_server(&mut v, &p, None));
        assert_eq!(v.turns[0].progress, Some(ProgressKind::Tool));
        assert_eq!(v.turns[0].tool.as_deref(), Some("ha_call_service"));
        on_server(&mut v, &token("t-1", "Vale, "), None);
        on_server(&mut v, &token("t-1", "pongo."), None);
        assert!(on_server(&mut v, &end_msg("t-1", None), None));
        let t = &v.turns[0];
        assert_eq!(
            (t.turn_id.as_str(), t.conversation.as_deref(), t.source),
            ("t-1", Some("c-1"), TurnSource::Mine)
        );
        assert_eq!(
            (t.mine.as_str(), t.reply.as_str()),
            ("pon la serie", "Vale, pongo.")
        );
        assert_eq!((t.progress, t.tool.as_deref()), (None, None));
        assert_eq!(t.outcome, Some(TurnOutcome::Done));
        // Late frames of an ended turn change nothing.
        assert!(!on_server(&mut v, &token("t-1", "x"), None));
        assert!(!on_server(
            &mut v,
            &end_msg("t-1", Some(TurnEndReason::Error)),
            None
        ));
    }

    #[test]
    fn outcomes_follow_turn_end_reason() {
        use TurnEndReason as R;
        for (reason, want) in [
            (None, TurnOutcome::Done),
            (Some(R::Done), TurnOutcome::Done),
            (Some(R::Error), TurnOutcome::Error),
            (Some(R::Cancelled), TurnOutcome::Cancelled),
            (Some(R::Interrupted), TurnOutcome::Interrupted),
            (Some(R::Replaced), TurnOutcome::Replaced),
            (Some(R::Timeout), TurnOutcome::Timeout),
            (Some(R::Closed), TurnOutcome::Closed),
        ] {
            let mut v = ConversationView::default();
            on_server(&mut v, &start("t-9", None), PUSH);
            on_server(&mut v, &end_msg("t-9", reason), None);
            assert_eq!(v.turns[0].outcome, Some(want), "{reason:?}");
            assert_eq!(v.turns[0].source, TurnSource::Push);
        }
    }

    #[test]
    fn empty_turn_id_never_matches() {
        let mut v = ConversationView::default();
        sent(&mut v, "hola");
        // My turn waits under an empty id: a token/turn_end/progress with an empty id must not
        // attach to it.
        assert!(!on_server(&mut v, &token("", "intruso"), None));
        assert!(!on_server(&mut v, &end_msg("", None), None));
        assert!(!on_server(&mut v, &err("X", "y", Some("")), None));
        assert_eq!((v.turns[0].reply.as_str(), v.turns[0].outcome), ("", None));
    }

    #[test]
    fn gateway_ids_and_codes_are_bounded() {
        let mut v = ConversationView::default();
        let long = format!("t-{}", "ñ".repeat(200));
        on_server(&mut v, &start(&long, Some(&long)), PUSH);
        let t = &v.turns[0];
        assert!(t.turn_id.len() <= ID_MAX_BYTES && long.starts_with(&t.turn_id));
        assert!(
            t.conversation
                .as_ref()
                .is_some_and(|c| c.len() <= ID_MAX_BYTES)
        );
        // The same over-long id still matches its (cut) turn.
        assert!(on_server(&mut v, &token(&long, "ok"), None));
        let code = "E".repeat(1000);
        assert!(on_server(&mut v, &err(&code, "m", Some(&long)), None));
        assert_eq!(
            v.turns[0].error_code.as_ref().map(String::len),
            Some(ID_MAX_BYTES)
        );
    }

    #[test]
    fn errors_before_and_during_a_turn() {
        let mut v = ConversationView::default();
        sent(&mut v, "a");
        on_server(
            &mut v,
            &err("TURN_ERROR", "orchestrator_unavailable", None),
            None,
        );
        assert_eq!(v.turns[0].outcome, Some(TurnOutcome::Error));
        assert_eq!(
            v.turns[0].error_code.as_deref(),
            Some("orchestrator_unavailable")
        );
        // An unknown reason keeps the code; a non-turn error without id touches nothing.
        sent(&mut v, "b");
        assert!(!on_server(&mut v, &err("INVALID_MESSAGE", "x", None), None));
        on_server(&mut v, &start("t-1", None), USER);
        on_server(&mut v, &err("TURN_ERROR", "free text", Some("t-1")), None);
        assert_eq!(v.turns[1].error_code.as_deref(), Some("TURN_ERROR"));
        // The first outcome wins over the turn_end that follows.
        on_server(&mut v, &end_msg("t-1", Some(TurnEndReason::Done)), None);
        assert_eq!(v.turns[1].outcome, Some(TurnOutcome::Error));
    }

    #[test]
    fn waits_end_and_a_new_text_replaces_the_waiting_one() {
        let mut v = ConversationView::default();
        sent(&mut v, "uno");
        sent(&mut v, "dos");
        assert_eq!(v.turns[0].outcome, Some(TurnOutcome::Replaced));
        assert!(wait_ended(&mut v, TurnOutcome::Timeout));
        assert_eq!(v.turns[1].outcome, Some(TurnOutcome::Timeout));
        assert!(!wait_ended(&mut v, TurnOutcome::Cancelled), "nothing waits");
        // A push while I wait does not take my text.
        sent(&mut v, "tres");
        on_server(&mut v, &start("t-1", None), PUSH);
        assert_eq!(
            (v.turns[2].turn_id.as_str(), v.turns[3].source),
            ("", TurnSource::Push)
        );
        on_server(&mut v, &start("t-2", None), USER);
        assert_eq!(v.turns[2].turn_id, "t-2");
    }

    #[test]
    fn link_down_closes_open_turns_and_forgets_voice_state() {
        let mut v = ConversationView::default();
        link_up(
            &mut v,
            Capabilities {
                barge_in: true,
                tts: true,
                transcriber: false,
            },
        );
        assert_eq!(
            (v.transcriber, v.tts),
            (ServiceState::Down, ServiceState::Up)
        );
        sent(&mut v, "a");
        on_server(&mut v, &start("t-1", None), PUSH);
        assert!(link_down(&mut v));
        assert!(
            v.turns
                .iter()
                .all(|t| t.outcome == Some(TurnOutcome::Closed))
        );
        assert_eq!(
            (v.transcriber, v.tts),
            (ServiceState::Unknown, ServiceState::Unknown)
        );
        assert!(!link_down(&mut v), "idempotent");
    }

    #[test]
    fn status_updates_voice_state_and_phrase_lost() {
        let mut v = ConversationView::default();
        for (service, state, want) in [
            ("tts", "unavailable", ServiceState::Down),
            ("tts", "reconnecting", ServiceState::Down),
            ("tts", "degraded", ServiceState::Degraded),
            ("tts", "restored", ServiceState::Up),
            ("tts", "something-new", ServiceState::Unknown),
        ] {
            on_server(&mut v, &status(service, state, None), None);
            assert_eq!(v.tts, want, "{state}");
        }
        assert!(!on_server(
            &mut v,
            &status("orchestrator", "unavailable", None),
            None
        ));
        on_server(
            &mut v,
            &status("transcriber", "degraded", Some("phrase_interrupted")),
            None,
        );
        assert_eq!(
            (v.transcriber, v.notice),
            (ServiceState::Degraded, Some(Notice::PhraseLost))
        );
        sent(&mut v, "otra vez");
        assert_eq!(v.notice, None, "cleared by the next action");
    }

    #[test]
    fn audio_played_marks_the_open_turn_once() {
        let mut v = ConversationView::default();
        on_server(&mut v, &start("t-1", None), PUSH);
        assert!(audio_played(&mut v, "t-1"));
        assert!(!audio_played(&mut v, "t-1"));
        assert!(!audio_played(&mut v, ""));
        assert_eq!(v.turns[0].audio, TurnAudio::Played);
    }

    #[test]
    fn bounds_drop_the_oldest_turns_and_cap_one_huge_reply() {
        let mut v = ConversationView::default();
        for i in 0..(CONVERSATION_MAX_TURNS + 5) {
            on_server(&mut v, &start(&format!("t-{i}"), None), PUSH);
        }
        assert_eq!(v.turns.len(), CONVERSATION_MAX_TURNS);
        assert_eq!(v.turns[0].turn_id, "t-5", "oldest dropped first");
        // Text bound across turns.
        let chunk = "x".repeat(8 * 1024);
        for i in 5..(CONVERSATION_MAX_TURNS + 5) {
            on_server(&mut v, &token(&format!("t-{i}"), &chunk), None);
        }
        assert!(text_bytes(&v) <= CONVERSATION_MAX_TEXT_BYTES);
        assert_eq!(v.turns.last().map(|t| t.reply.len()), Some(chunk.len()));
        // One reply alone over the bound is cut at a char boundary.
        let mut v = ConversationView::default();
        on_server(&mut v, &start("t-1", None), PUSH);
        for _ in 0..20 {
            on_server(&mut v, &token("t-1", &"é".repeat(2000)), None);
        }
        assert_eq!(v.turns.len(), 1);
        assert!(text_bytes(&v) <= CONVERSATION_MAX_TEXT_BYTES);
        assert!(v.turns[0].reply.chars().all(|c| c == 'é'));
    }

    #[test]
    fn debug_prints_sizes_never_content() {
        let mut v = ConversationView::default();
        set_activity(&mut v, UserActivity::AwaitingReply);
        sent(&mut v, "mi PIN es 4321");
        on_server(&mut v, &start("t-1", None), USER);
        on_server(&mut v, &token("t-1", "secreto 9876"), None);
        let d = format!("{v:?}");
        assert!(!d.contains("4321") && !d.contains("9876"), "{d}");
        assert!(d.contains("mine_bytes") && d.contains("reply_bytes"), "{d}");
    }
}
