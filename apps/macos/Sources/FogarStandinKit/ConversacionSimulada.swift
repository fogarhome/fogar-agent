import FogarWire
import Foundation

// The stand-in agent's conversation (IPC v4): the same rules the core applies, as a pure state
// machine with no clock and no socket, so `swift test` can check it. `fogar-standin` drives it with
// timers (the scripted replies, partials and deadlines live there). Synthetic data only.

/// Limits mirrored from `fogar_ports::ipc` (SEND_TEXT_MAX_BYTES, CONVERSATION_MAX_TURNS,
/// CONVERSATION_MAX_TEXT_BYTES).
public enum LimitesConversacion {
    public static let textoMaxBytes = 8 * 1024
    public static let turnosMax = 20
    public static let textoVistaMaxBytes = 48 * 1024
}

/// The answer the core gives to a conversation request from a peer that is not the signed app.
public let soloAppFirmada = WireResponse.refused(WireRefused(reason: "needs the signed Fogar app"))

/// What the agent knows of the conversation, and the single consent and output points' rules.
public struct ConversacionSimulada: Sendable {
    public var activity = WireActivity.idle
    public var live = ""
    public var draft: String?
    public private(set) var turns: [WireTurn] = []
    public private(set) var speaking = false
    public private(set) var awaitingVisible: String?
    public var mic: WireMic
    public var transcriber: WireService
    public var tts: WireService
    public var notice: WireNotice?
    /// Whether the link to the gateway is ready (text and takes need it).
    public var listo: Bool
    /// This machine has no microphone the agent can use.
    public var sinMicro = false
    /// The connection that owns the open take (it ends with `discard` if that connection goes).
    public private(set) var tomaDe: Int?
    /// Turns confirmed on screen by the app (`turn_visible`): only their audio may play.
    public private(set) var visibles: Set<String> = []
    /// Audio dropped because nobody confirmed the turn in time (the core's counter).
    public private(set) var audiosTirados = 0
    private var siguiente = 1
    private var conversacionDelDia = "conv-2026-11-22"

    public init(mic: WireMic = .notDetermined, transcriber: WireService = .up, tts: WireService = .up, listo: Bool = true) {
        self.mic = mic
        self.transcriber = transcriber
        self.tts = tts
        self.listo = listo
    }

    public var vista: WireConversation {
        WireConversation(activity: activity, live: live, draft: draft, turns: turns, speaking: speaking,
                         awaiting_visible: awaitingVisible, mic: mic, transcriber: transcriber, tts: tts, notice: notice)
    }

    public func turno(_ id: String) -> WireTurn? { turns.first { $0.turn_id == id } }
    public var turnoAbierto: WireTurn? { turns.last { $0.outcome == nil } }

    // MARK: Requests (the core's answers)

    /// `send_text`: validated like `fogar-core::ipc::check_text`, then the engine's checks.
    public mutating func enviarTexto(_ texto: String) -> WireResponse {
        if texto.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { return .textRefused(WireTextRefused(reason: .empty)) }
        if texto.utf8.count > LimitesConversacion.textoMaxBytes { return .textRefused(WireTextRefused(reason: .tooLong)) }
        if !listo { return .textRefused(WireTextRefused(reason: .notReady)) }
        if activity == .recording { return .textRefused(WireTextRefused(reason: .recording)) }
        notice = nil
        draft = nil
        activity = .awaitingReply
        return .textAccepted
    }

    /// `capture_start` from connection `conexion` (subscribed or not): the single consent point.
    public mutating func empezarToma(conexion: Int, suscrita: Bool) -> WireResponse {
        let motivo: WireCaptureRefusal? =
            if !suscrita { .notSubscribed }
            else if tomaDe != nil || activity == .recording { .busy }
            else if mic == .notDetermined { .permissionNotDetermined }
            else if mic == .denied || mic == .restricted { .permissionDenied }
            else if sinMicro { .noMicrophone }
            else if !listo { .notReady }
            else if transcriber == .down { .noTranscriber }
            else { nil }
        if let motivo { return .captureRefused(WireCaptureRefused(reason: motivo)) }
        tomaDe = conexion
        activity = .recording
        live = ""
        notice = nil
        return .captureStarted
    }

    /// A `transcription_partial` while recording.
    public mutating func parcial(_ texto: String) {
        guard activity == .recording else { return }
        live = texto
    }

    /// `capture_end` from connection `conexion`: only the take's owner ends it; idempotent.
    /// Returns the transcript to send when `then` is `send`.
    public mutating func terminarToma(_ then: WireThen, conexion: Int) -> (respuesta: WireResponse, enviar: String?) {
        guard tomaDe == conexion else { return (.captureEnded, nil) }
        return terminarToma(then)
    }

    /// Ends the open take whoever owns it (the agent's own cuts and a lost connection).
    mutating func terminarToma(_ then: WireThen) -> (respuesta: WireResponse, enviar: String?) {
        guard activity == .recording else { return (.captureEnded, nil) }
        let texto = live.trimmingCharacters(in: .whitespacesAndNewlines)
        tomaDe = nil
        live = ""
        switch then {
        case .discard:
            activity = .idle
            return (.captureEnded, nil)
        case .review:
            activity = .idle
            draft = texto.isEmpty ? nil : texto
            return (.captureEnded, nil)
        case .send:
            guard !texto.isEmpty else { activity = .idle; return (.captureEnded, nil) }
            activity = .awaitingReply
            return (.captureEnded, texto)
        }
    }

    /// The agent cut the take on its own (silence or safety limit): to review, never sent.
    public mutating func cortarToma(_ motivo: WireNotice) {
        guard activity == .recording else { return }
        _ = terminarToma(.review)
        notice = motivo
    }

    /// The connection that owned the take closed or stopped being subscribed: discard.
    public mutating func conexionPerdida(_ conexion: Int) {
        if tomaDe == conexion { _ = terminarToma(.discard) }
    }

    /// `cancel_turn`: `cancel` to the gateway and silence at once.
    public mutating func cancelar() -> WireResponse {
        speaking = false
        if activity == .awaitingReply || activity == .transcribing { activity = .idle }
        if let t = turnoAbierto { cerrar(t.turn_id, .cancelled) }
        awaitingVisible = nil
        return .turnCancelSent
    }

    /// `turn_visible`: the app shows this turn, so its audio may play. Only from a connection
    /// subscribed to the conversation (GUESSED answer for any other: `refused`).
    public mutating func turnoVisible(_ id: String, suscrita: Bool = true) -> WireResponse {
        guard suscrita else { return .refused(WireRefused(reason: "not subscribed")) }
        guard turno(id) != nil else { return .turnVisibleNoted }
        visibles.insert(id)
        if awaitingVisible == id {
            awaitingVisible = nil
            if turno(id)?.outcome == nil { speaking = true }
        }
        return .turnVisibleNoted
    }

    /// `request_mic_permission`: the system prompt, answered `concede` or not.
    public mutating func pedirPermiso(concede: Bool) -> WireResponse {
        if mic == .notDetermined { mic = concede ? .granted : .denied }
        return .micPermission(WireMicPermission(state: mic))
    }

    // MARK: The gateway's side (scripted by the stand-in)

    /// `turn_start`: a new turn, mine (with what I said) or a push.
    public mutating func abrirTurno(source: WireSource, mine: String) -> String {
        let id = "t\(siguiente)"
        siguiente += 1
        if activity == .awaitingReply { activity = .idle }
        turns.append(WireTurn(turn_id: id, conversation: conversacionDelDia, source: source, mine: mine, reply: "",
                              progress: .preparing, tool: nil, outcome: nil, error_code: nil, audio: .none))
        recortar()
        return id
    }

    /// A new day: later turns run in another gateway conversation.
    public mutating func nuevoDia(_ conversacion: String) { conversacionDelDia = conversacion }

    public mutating func progreso(_ id: String, _ p: WireProgress, herramienta: String? = nil) {
        cambiar(id) { $0.progress = p; $0.tool = p == .tool ? herramienta : nil }
    }

    /// Reply tokens: progress clears once text flows.
    public mutating func texto(_ id: String, _ trozo: String) {
        cambiar(id) { $0.reply += trozo; $0.progress = nil; $0.tool = nil }
        recortar()
    }

    /// The first audio of a turn arrives: it plays only if the app already confirmed the turn;
    /// otherwise it waits (bounded, see `plazoVencido`).
    public mutating func llegaAudio(_ id: String) {
        guard let t = turno(id), t.outcome == nil, t.audio == .none, tts != .down else { return }
        if visibles.contains(id) { speaking = true } else { awaitingVisible = id }
    }

    /// 2 s passed with no `turn_visible` (or the 3 s buffer filled): the turn stays text only.
    public mutating func plazoVencido(_ id: String) {
        guard awaitingVisible == id else { return }
        awaitingVisible = nil
        audiosTirados += 1
        cambiar(id) { $0.audio = .muted }
    }

    /// The audio of the turn finished playing.
    public mutating func audioTermina(_ id: String) {
        guard speaking, turno(id)?.audio == WireAudio.none else { return }
        speaking = false
        cambiar(id) { $0.audio = .played }
    }

    /// `turn_end`.
    public mutating func cerrar(_ id: String, _ outcome: WireOutcome, codigo: String? = nil) {
        guard turno(id)?.outcome == nil else { return }
        let esperaba = awaitingVisible == id
        cambiar(id) {
            $0.outcome = outcome
            $0.progress = nil
            $0.tool = nil
            $0.error_code = outcome == .error ? codigo : nil
            if esperaba { $0.audio = .muted }
        }
        if esperaba { awaitingVisible = nil; audiosTirados += 1 }
        if outcome != .done { speaking = false }
    }

    // MARK: Helpers

    private mutating func cambiar(_ id: String, _ f: (inout Turno) -> Void) {
        guard let i = turns.firstIndex(where: { $0.turn_id == id }) else { return }
        var t = Turno(turns[i])
        f(&t)
        turns[i] = t.wire
    }

    /// Bounded like the core's view: at most 20 turns and 48 KB of text, oldest dropped first.
    private mutating func recortar() {
        while turns.count > LimitesConversacion.turnosMax { turns.removeFirst() }
        while turns.count > 1, turns.reduce(0, { $0 + $1.mine.utf8.count + $1.reply.utf8.count }) > LimitesConversacion.textoVistaMaxBytes {
            turns.removeFirst()
        }
    }
}

/// A mutable copy of a `WireTurn` (the generated struct has `let` fields).
struct Turno {
    var turn_id: String
    var conversation: String?
    var source: WireSource
    var mine: String
    var reply: String
    var progress: WireProgress?
    var tool: String?
    var outcome: WireOutcome?
    var error_code: String?
    var audio: WireAudio

    init(_ t: WireTurn) {
        turn_id = t.turn_id; conversation = t.conversation; source = t.source; mine = t.mine; reply = t.reply
        progress = t.progress; tool = t.tool; outcome = t.outcome; error_code = t.error_code; audio = t.audio
    }

    var wire: WireTurn {
        WireTurn(turn_id: turn_id, conversation: conversation, source: source, mine: mine, reply: reply,
                 progress: progress, tool: tool, outcome: outcome, error_code: error_code, audio: audio)
    }
}
