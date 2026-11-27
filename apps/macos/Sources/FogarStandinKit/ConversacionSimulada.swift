import FogarWire
import Foundation

// The stand-in agent's conversations (IPC v6): the same rules the core applies, as a pure state
// machine with no clock and no socket, so `swift test` can check it. `fogar-standin` drives it with
// timers (the scripted replies, partials and deadlines live there). Synthetic data only.

/// Limits mirrored from `fogar_ports::ipc` / `fogar_core::conversation` (SEND_TEXT_MAX_BYTES,
/// MAX_ENTRIES, CONVERSATION_MAX_TEXT_BYTES, LIST_MAX).
public enum LimitesConversacion {
    public static let textoMaxBytes = 8 * 1024
    public static let entradasMax = 40
    public static let textoVistaMaxBytes = 48 * 1024
    public static let listaMax = 50
}

/// The answer the core gives to a conversation request from a peer that is not the signed app.
public let soloAppFirmada = WireResponse.refused(WireRefused(reason: "needs the signed Fogar app"))

/// What the agent knows of the conversations, and the single consent point's rules.
public struct ConversacionSimulada: Sendable {
    public var activity = WireActivity.idle
    public var live = ""
    public var draft: String?
    /// The conversation on screen; `nil` = none (the next text starts one).
    public private(set) var abierta: String?
    /// Newest first, like the core's list.
    public private(set) var lista: [WireSummary] = []
    /// Every conversation's entries (the stand-in keeps them all; the core fetches on open).
    private var hilos: [String: [WireEntry]] = [:]
    public var mic: WireMic
    public var transcriber: WireService
    /// Shown in the status only (nothing plays over `/events`).
    public var tts: WireService
    public var notice: WireNotice?
    /// The account lacks `conversations:write`.
    public var soloLectura = false
    /// Whether the link to the gateway is ready (text and takes need it).
    public var listo: Bool
    /// This machine has no microphone the agent can use.
    public var sinMicro = false
    /// The connection that owns the open take (it ends with `discard` if that connection goes).
    public private(set) var tomaDe: Int?
    private var siguiente = 1

    public init(mic: WireMic = .notDetermined, transcriber: WireService = .up, tts: WireService = .up, listo: Bool = true,
                ejemplos: Bool = false) {
        self.mic = mic
        self.transcriber = transcriber
        self.tts = tts
        self.listo = listo
        if ejemplos {
            crear("conv-compra", titulo: "Lista de la compra", privada: true, cuando: "2026-11-26T19:12:00Z")
            hilos["conv-compra"] = [
                .message(WireMessage(id: "", text: "Añade leche y huevos", mine: false, from_name: nil, state: .past, error_code: nil)),
                .turn(WireTurn(turn_id: "", source: .mine, by_name: nil, by_self: false, reply: "Añadidos: leche y huevos.",
                               progress: nil, tool: nil, outcome: .done, error_code: nil)),
            ]
            crear("conv-serie", titulo: "La serie de esta noche", privada: false, cuando: "2026-11-27T09:40:00Z")
            hilos["conv-serie"] = [
                .message(WireMessage(id: "", text: "Pon la serie en el salón", mine: false, from_name: nil, state: .past, error_code: nil)),
                .turn(WireTurn(turn_id: "", source: .mine, by_name: nil, by_self: false, reply: "Puesta en la tele del salón.",
                               progress: nil, tool: nil, outcome: .done, error_code: nil)),
            ]
        }
    }

    public var entries: [WireEntry] { abierta.flatMap { hilos[$0] } ?? [] }

    public var vista: WireConversation {
        WireConversation(id: abierta, activity: activity, live: live, draft: draft, entries: entries, mic: mic,
                         transcriber: transcriber, notice: notice, read_only: soloLectura)
    }

    public func turno(_ id: String) -> WireTurn? {
        for (_, h) in hilos {
            for case .turn(let t) in h where t.turn_id == id { return t }
        }
        return nil
    }

    /// The turn running in `conversacion`, if any.
    public func turnoAbierto(en conversacion: String) -> WireTurn? {
        for case .turn(let t) in hilos[conversacion] ?? [] where t.outcome == nil && !t.turn_id.isEmpty { return t }
        return nil
    }

    /// Where turn `id` runs.
    public func conversacion(de id: String) -> String? {
        hilos.first { _, h in h.contains { if case .turn(let t) = $0 { t.turn_id == id } else { false } } }?.key
    }

    // MARK: Requests (the core's answers)

    /// `send_text`: validated like `fogar-core::ipc::check_text`, then the engine's checks. With
    /// nothing open it creates a conversation (it opens). Returns where the text went, and whether
    /// it reached the agent at once (`delivered`) or waits behind a running turn (`pending`).
    public mutating func enviarTexto(_ texto: String) -> (respuesta: WireResponse, en: String?, entregado: Bool) {
        if texto.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { return (.textRefused(WireTextRefused(reason: .empty)), nil, false) }
        if texto.utf8.count > LimitesConversacion.textoMaxBytes { return (.textRefused(WireTextRefused(reason: .tooLong)), nil, false) }
        if activity == .recording { return (.textRefused(WireTextRefused(reason: .recording)), nil, false) }
        if soloLectura { return (.textRefused(WireTextRefused(reason: .missingScope)), nil, false) }
        if !listo { return (.textRefused(WireTextRefused(reason: .notReady)), nil, false) }
        notice = nil
        draft = nil
        let conv: String
        if let a = abierta {
            conv = a
        } else {
            conv = "conv-\(siguiente)"
            siguiente += 1
            crear(conv, titulo: String(texto.prefix(40)), privada: true, cuando: nil)
            abierta = conv
        }
        let entregado = mensaje(texto, en: conv, mio: true, de: nil)
        return (.textAccepted, conv, entregado)
    }

    /// A person's message in `conv` (another device's when `mio` is false). `true` = delivered (no
    /// turn runs there), else pending behind it.
    @discardableResult
    public mutating func mensaje(_ texto: String, en conv: String, mio: Bool, de nombre: String?) -> Bool {
        let entregado = turnoAbierto(en: conv) == nil
        let id = "m\(siguiente)"
        siguiente += 1
        anadir(.message(WireMessage(id: id, text: texto, mine: mio, from_name: mio ? nil : nombre,
                                    state: entregado ? .delivered : .pending, error_code: nil)), en: conv)
        return entregado
    }

    /// `capture_start` from connection `conexion` (subscribed or not): the single consent point.
    /// Dictation is off until PR 5: no gateway transcriber is ever `up`, so a ready Mac is refused
    /// `no_transcriber` (`--transcriber up` lets the take's look be developed).
    public mutating func empezarToma(conexion: Int, suscrita: Bool) -> WireResponse {
        let motivo: WireCaptureRefusal? =
            if !suscrita { .notSubscribed }
            else if tomaDe != nil || activity == .recording { .busy }
            else if mic == .notDetermined { .permissionNotDetermined }
            else if mic == .denied || mic == .restricted { .permissionDenied }
            else if sinMicro { .noMicrophone }
            else if !listo { .notReady }
            else if transcriber != .up { .noTranscriber }
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
        activity = .idle
        switch then {
        case .discard:
            return (.captureEnded, nil)
        case .review:
            draft = texto.isEmpty ? nil : texto
            return (.captureEnded, nil)
        case .send:
            return (.captureEnded, texto.isEmpty ? nil : texto)
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

    /// `cancel_turn`: the open conversation's turn ends `cancelled`, and its pending messages are
    /// withdrawn (they disappear, as in the gateway).
    public mutating func cancelar() -> WireResponse {
        guard let conv = abierta else { return .turnCancelSent }
        // Withdrawn first: ending the turn would deliver the next pending message.
        hilos[conv]?.removeAll { if case .message(let m) = $0 { m.state == .pending } else { false } }
        if let t = turnoAbierto(en: conv) { cerrar(t.turn_id, .cancelled) }
        return .turnCancelSent
    }

    /// `open_conversation`.
    public mutating func abrir(_ id: String) -> WireResponse {
        guard lista.contains(where: { $0.id == id }) else { return .refused(WireRefused(reason: "unknown conversation")) }
        abierta = id
        return .conversationOpened
    }

    /// `new_conversation`: nothing open.
    public mutating func nueva() -> WireResponse {
        abierta = nil
        return .conversationOpened
    }

    /// `request_mic_permission`: the system prompt, answered `concede` or not.
    public mutating func pedirPermiso(concede: Bool) -> WireResponse {
        if mic == .notDetermined { mic = concede ? .granted : .denied }
        return .micPermission(WireMicPermission(state: mic))
    }

    // MARK: The gateway's side (scripted by the stand-in)

    /// `turn_start` in `conv`: a person's turn (`por`: the device that started it, `nil` = this
    /// Mac) or a push. The conversation is marked «en marcha» and goes to the top.
    public mutating func abrirTurno(source: WireSource, en conv: String, por nombre: String? = nil) -> String {
        let id = "t\(siguiente)"
        siguiente += 1
        anadir(.turn(WireTurn(turn_id: id, source: source, by_name: source == .push ? nil : nombre,
                              by_self: source == .mine && nombre == nil, reply: "", progress: .preparing,
                              tool: nil, outcome: nil, error_code: nil)), en: conv)
        marcar(conv, enMarcha: true)
        return id
    }

    public mutating func progreso(_ id: String, _ p: WireProgress, herramienta: String? = nil) {
        cambiar(id) { $0.progress = p; $0.tool = p == .tool ? herramienta : nil }
    }

    /// Reply tokens: progress clears once text flows.
    public mutating func texto(_ id: String, _ trozo: String) {
        cambiar(id) { $0.reply += trozo; $0.progress = nil; $0.tool = nil }
    }

    /// `turn_end`. The first message waiting in that conversation is delivered: its text is
    /// returned (the stand-in answers it next).
    @discardableResult
    public mutating func cerrar(_ id: String, _ outcome: WireOutcome, codigo: String? = nil) -> String? {
        guard turno(id)?.outcome == nil, let conv = conversacion(de: id) else { return nil }
        cambiar(id) {
            $0.outcome = outcome
            $0.progress = nil
            $0.tool = nil
            $0.error_code = outcome == .error ? codigo : nil
        }
        marcar(conv, enMarcha: false)
        guard let i = hilos[conv]?.firstIndex(where: { if case .message(let m) = $0 { m.state == .pending } else { false } }),
              case .message(let m)? = hilos[conv]?[i]
        else { return nil }
        hilos[conv]?[i] = .message(WireMessage(id: m.id, text: m.text, mine: m.mine, from_name: m.from_name,
                                               state: .delivered, error_code: nil))
        return m.text
    }

    // MARK: Helpers

    private mutating func crear(_ id: String, titulo: String, privada: Bool, cuando: String?) {
        lista.insert(WireSummary(id: id, title: titulo, private: privada, last_turn_at: cuando, running: false), at: 0)
        if lista.count > LimitesConversacion.listaMax { lista.removeLast() }
        hilos[id] = hilos[id] ?? []
    }

    private mutating func marcar(_ conv: String, enMarcha: Bool) {
        guard let i = lista.firstIndex(where: { $0.id == conv }) else { return }
        let c = lista.remove(at: i)
        let nueva = WireSummary(id: c.id, title: c.title, private: c.private,
                                last_turn_at: enMarcha ? c.last_turn_at : "2026-11-27T12:00:00Z", running: enMarcha)
        lista.insert(nueva, at: enMarcha ? 0 : i)
    }

    private mutating func anadir(_ e: WireEntry, en conv: String) {
        var h = hilos[conv] ?? []
        h.append(e)
        // Bounded like the core's store: at most 40 entries and 48 KB of text, oldest first.
        while h.count > LimitesConversacion.entradasMax { h.removeFirst() }
        while h.count > 1, h.reduce(0, { $0 + Self.bytes($1) }) > LimitesConversacion.textoVistaMaxBytes { h.removeFirst() }
        hilos[conv] = h
    }

    static func bytes(_ e: WireEntry) -> Int {
        switch e {
        case .message(let m): m.text.utf8.count
        case .turn(let t): t.reply.utf8.count
        }
    }

    private mutating func cambiar(_ id: String, _ f: (inout Turno) -> Void) {
        guard let conv = conversacion(de: id), var h = hilos[conv],
              let i = h.firstIndex(where: { if case .turn(let t) = $0 { t.turn_id == id } else { false } }),
              case .turn(let w) = h[i]
        else { return }
        var t = Turno(w)
        f(&t)
        h[i] = .turn(t.wire)
        hilos[conv] = h
        // The text bound holds as replies grow, too.
        while h.count > 1, h.reduce(0, { $0 + Self.bytes($1) }) > LimitesConversacion.textoVistaMaxBytes {
            h.removeFirst()
            hilos[conv] = h
        }
    }
}

/// A mutable copy of a `WireTurn` (the generated struct has `let` fields).
struct Turno {
    var w: WireTurn
    var reply: String
    var progress: WireProgress?
    var tool: String?
    var outcome: WireOutcome?
    var error_code: String?

    init(_ t: WireTurn) {
        w = t; reply = t.reply; progress = t.progress; tool = t.tool; outcome = t.outcome; error_code = t.error_code
    }

    var wire: WireTurn {
        WireTurn(turn_id: w.turn_id, source: w.source, by_name: w.by_name, by_self: w.by_self, reply: reply,
                 progress: progress, tool: tool, outcome: outcome, error_code: error_code)
    }
}
