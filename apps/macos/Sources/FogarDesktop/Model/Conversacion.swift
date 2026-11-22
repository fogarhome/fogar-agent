import AppKit
import FogarWire
import Foundation
import Observation

/// What the microphone button can do right now. One sentence each in `Textos`.
enum EstadoMicro: Equatable {
    case listo
    /// The conversation connection is down, or the agent is not linked to the gateway.
    case sinConexion
    /// The agent only talks about the conversation with the app signed like it.
    case soloAppFirmada
    /// The gateway has no live transcriber: text still works.
    case sinTranscriptor
    /// macOS never asked: the button asks (`request_mic_permission`).
    case pedirPermiso
    /// Denied or restricted in System Settings.
    case denegado
}

/// The take as this app drives it. The agent's `activity` is the truth about the microphone; this
/// only remembers what the person's hands are doing.
enum Toma: Equatable {
    case ninguna
    /// `capture_start` sent, no answer yet. `pulsado`: the pointer is still down. `fin`: how to end
    /// it as soon as the agent says it opened (a hold already released, Esc, «Enviar ya»).
    case abriendo(desde: Date, pulsado: Bool, fin: WireThen?)
    /// Open. `mantener`: the button is held (release sends); otherwise clicked (three buttons).
    case abierta(mantener: Bool, desde: Date, pulsado: Bool)
    /// `capture_end` sent.
    case cerrando
}

/// The conversation section's model: the text box, the take's gestures, the visible-turn
/// confirmations and the one-line feedback. Everything it shows comes from the agent's view.
@Observable
final class Conversacion {
    /// A press held this long is «mantener» (release sends); shorter is a click.
    static let umbralMantener: TimeInterval = 0.3

    let link: ConversationLink
    /// The text box.
    var texto = ""
    private(set) var toma: Toma = .ninguna
    /// One sentence about the last thing that went wrong (cleared by the next action).
    private(set) var aviso: String?
    private(set) var enviando = false
    /// Whether the conversation is on screen (section selected, window visible): only then is a
    /// push confirmed, so only then can it sound.
    var aLaVista = false {
        didSet { if aLaVista, !oldValue, let v = link.vista { confirmar(v) } }
    }
    /// The draft last copied into the box, so the same draft is not copied twice.
    @ObservationIgnored private var ultimoBorrador: String?
    /// Turn ids already confirmed (bounded: the view never holds more than 20 turns).
    @ObservationIgnored private var confirmados: [String] = []
    /// The agent showed `recording` during this take (so a later `idle` means the take ended).
    @ObservationIgnored private var vioGrabando = false
    /// Bumped when the draft lands in the box (the view focuses the field).
    private(set) var borradorNuevo = 0

    init(link: ConversationLink) {
        self.link = link
        link.alCambiar = { [weak self] v in self?.actualizar(v) }
        link.alPerder = { [weak self] in self?.perdido() }
    }

    var vista: WireConversation? { link.vista }

    // MARK: Derived state

    /// Whether the agent's microphone is open for this take.
    var grabando: Bool { vivo && vista?.activity == .recording }

    /// The conversation connection is up (otherwise the view on screen is the last one seen).
    var vivo: Bool { link.enlace == .viva }

    /// A turn is open, a reply is awaited or audio plays: «Cancelar» makes sense.
    var cancelable: Bool {
        guard link.enlace == .viva, let v = vista else { return false }
        return Self.cancelable(v)
    }

    static func cancelable(_ v: WireConversation) -> Bool {
        v.speaking || v.activity == .awaitingReply || v.awaiting_visible != nil || v.turns.contains { $0.outcome == nil }
    }

    func estadoMicro(listo: Bool) -> EstadoMicro {
        Self.estadoMicro(enlace: link.enlace, vista: vista, listo: listo)
    }

    /// `listo`: the agent's status says the link to the gateway is ready.
    static func estadoMicro(enlace: EnlaceConversacion, vista: WireConversation?, listo: Bool) -> EstadoMicro {
        switch enlace {
        case .soloAppFirmada: return .soloAppFirmada
        case .viva: break
        default: return .sinConexion
        }
        guard let v = vista, listo else { return .sinConexion }
        switch v.mic {
        case .notDetermined: return .pedirPermiso
        case .denied, .restricted: return .denegado
        case .granted, .unknown: break
        }
        return v.transcriber == .down ? .sinTranscriptor : .listo
    }

    /// Text that may be sent now (trimmed, within the agent's limit, no take open).
    var puedeEnviar: Bool {
        let t = texto.trimmingCharacters(in: .whitespacesAndNewlines)
        return !t.isEmpty && link.enlace == .viva && !grabando && toma == .ninguna && !enviando
    }

    // MARK: The agent's view

    private func actualizar(_ v: WireConversation) {
        // The draft (a reviewed take or an automatic cut) goes to the box, never sent on its own.
        if let d = v.draft, d != ultimoBorrador {
            ultimoBorrador = d
            let actual = texto.trimmingCharacters(in: .whitespacesAndNewlines)
            texto = actual.isEmpty ? d : actual + " " + d
            borradorNuevo += 1
        } else if v.draft == nil {
            ultimoBorrador = nil
        }
        // The agent ended the take on its own (silence, safety limit, connection): forget the hands.
        if v.activity == .recording {
            vioGrabando = true
        } else if vioGrabando {
            vioGrabando = false
            if case .abierta = toma { toma = .ninguna }
            if toma == .cerrando { toma = .ninguna }
        }
        confirmar(v)
    }

    /// `turn_visible` for what is on screen: my turns always (I just wrote or spoke, and the menu
    /// bar says «hablando»), a push only while the conversation is in view.
    private func confirmar(_ v: WireConversation) {
        for id in Self.porConfirmar(v, aLaVista: aLaVista, ya: Set(confirmados)) {
            confirmados.append(id)
            if confirmados.count > 64 { confirmados.removeFirst(confirmados.count - 64) }
            Task { _ = try? await link.pedir(.turnVisible(WireTurnVisible(turn_id: id))) }
        }
    }

    static func porConfirmar(_ v: WireConversation, aLaVista: Bool, ya: Set<String>) -> [String] {
        var ids: [String] = []
        func elegible(_ t: WireTurn) -> Bool { t.source == .mine || aLaVista }
        for t in v.turns where t.outcome == nil && elegible(t) && !ya.contains(t.turn_id) {
            ids.append(t.turn_id)
        }
        if let id = v.awaiting_visible, !ya.contains(id), !ids.contains(id),
           let t = v.turns.first(where: { $0.turn_id == id }), elegible(t) {
            ids.append(id)
        }
        return ids
    }

    // MARK: Text

    func enviar() async {
        guard puedeEnviar else { return }
        let t = texto.trimmingCharacters(in: .whitespacesAndNewlines)
        guard t.utf8.count <= Self.textoMaxBytes else { aviso = Textos.rechazoTexto(.tooLong); return }
        enviando = true
        aviso = nil
        defer { enviando = false }
        do {
            switch try await link.pedir(.sendText(WireSendText(text: t))) {
            case .textAccepted:
                // Only now: a refused text stays in the box to try again.
                if texto.trimmingCharacters(in: .whitespacesAndNewlines) == t { texto = "" }
            case .textRefused(let r): aviso = Textos.rechazoTexto(r.reason)
            case .refused: aviso = Textos.conversacionOcupada
            default: aviso = Textos.fallo(CommandError.unexpected)
            }
        } catch {
            aviso = Textos.fallo(error)
        }
    }

    /// `fogar_ports::ipc::SEND_TEXT_MAX_BYTES`.
    static let textoMaxBytes = 8 * 1024

    func cancelar() async {
        aviso = nil
        do {
            switch try await link.pedir(.cancelTurn) {
            case .turnCancelSent: break
            case .refused: aviso = Textos.conversacionOcupada
            default: aviso = Textos.fallo(CommandError.unexpected)
            }
        } catch {
            aviso = Textos.fallo(error)
        }
    }

    // MARK: The take

    /// The pointer went down on the microphone button: the take opens at once (no speech lost);
    /// the release decides whether it was a click or a hold.
    func pulsar(ahora: Date = Date()) {
        guard toma == .ninguna, !grabando else { return }
        empezar(ahora: ahora)
    }

    /// The pointer came up. Held past the threshold: send. A click leaves the take open.
    func soltar(ahora: Date = Date()) {
        switch toma {
        case .abriendo(let desde, true, let fin):
            let mantenido = ahora.timeIntervalSince(desde) >= Self.umbralMantener
            toma = .abriendo(desde: desde, pulsado: false, fin: mantenido ? .send : fin)
        case .abierta(_, let desde, true) where ahora.timeIntervalSince(desde) >= Self.umbralMantener:
            terminar(.send)
        case .abierta(_, let desde, true):
            toma = .abierta(mantener: false, desde: desde, pulsado: false)
        default:
            break
        }
    }

    /// Called while the pointer stays down: past the threshold the take shows «suelta para enviar».
    func comprobarMantener(ahora: Date = Date()) {
        if case .abierta(false, let desde, true) = toma, ahora.timeIntervalSince(desde) >= Self.umbralMantener {
            toma = .abierta(mantener: true, desde: desde, pulsado: true)
        }
    }

    /// Keyboard, VoiceOver or the accessibility action: start a clicked take, or stop one to review.
    func alternar() {
        switch toma {
        case .ninguna where !grabando: empezar(ahora: Date(), pulsado: false)
        case .abierta: terminar(.review)
        default: break
        }
    }

    func parar() { terminar(.review) }
    func enviarYa() { terminar(.send) }
    func descartar() { terminar(.discard) }

    private func empezar(ahora: Date, pulsado: Bool = true) {
        aviso = nil
        toma = .abriendo(desde: ahora, pulsado: pulsado, fin: nil)
        Task {
            do {
                // GUESSED: the gesture only feeds the agent's metrics and is not known before the
                // release (the take opens on press so no speech is lost): reported `toggle`.
                switch try await link.pedir(.captureStart(WireCaptureStart(gesture: .toggle))) {
                case .captureStarted:
                    guard case .abriendo(let desde, let pulsado, let fin) = toma else {
                        // The link dropped meanwhile: make sure nothing stays open.
                        enviarFin(.discard)
                        return
                    }
                    if let fin {
                        toma = .cerrando
                        enviarFin(fin)
                    } else {
                        toma = .abierta(mantener: false, desde: desde, pulsado: pulsado)
                        comprobarMantener()
                    }
                case .captureRefused(let r):
                    toma = .ninguna
                    aviso = Textos.rechazoToma(r.reason)
                case .refused:
                    toma = .ninguna
                    aviso = Textos.conversacionOcupada
                default:
                    toma = .ninguna
                    aviso = Textos.fallo(CommandError.unexpected)
                }
            } catch {
                toma = .ninguna
                aviso = Textos.fallo(error)
            }
        }
    }

    private func terminar(_ then: WireThen) {
        switch toma {
        case .abierta:
            toma = .cerrando
            enviarFin(then)
        case .abriendo(let desde, let pulsado, _):
            toma = .abriendo(desde: desde, pulsado: pulsado, fin: then)
        case .ninguna where grabando:
            enviarFin(then)
        default:
            break
        }
    }

    private func enviarFin(_ then: WireThen) {
        Task {
            do {
                _ = try await link.pedir(.captureEnd(WireCaptureEnd(then: then)))
            } catch {
                aviso = Textos.fallo(error)
            }
            if toma == .cerrando { toma = .ninguna }
        }
    }

    /// The conversation connection dropped: the agent ended any take with it (`discard`).
    private func perdido() {
        toma = .ninguna
        vioGrabando = false
    }

    // MARK: Permission

    func pedirPermiso() async {
        aviso = nil
        do {
            switch try await link.pedir(.requestMicPermission, plazo: .seconds(120)) {
            case .micPermission(let p) where p.state == .denied || p.state == .restricted:
                aviso = Textos.microDenegado
            case .micPermission: break
            case .refused: aviso = Textos.conversacionOcupada
            default: aviso = Textos.fallo(CommandError.unexpected)
            }
        } catch {
            aviso = Textos.fallo(error)
        }
    }

    /// System Settings → Privacy & Security → Microphone.
    static let ajustesMicro = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone")

    func abrirAjustesMicro() {
        if let url = Self.ajustesMicro { NSWorkspace.shared.open(url) }
    }

    func olvidarAviso() { aviso = nil }
}
