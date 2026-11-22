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
    /// The agent does not know the permission (`unknown`): no take is offered.
    case desconocido
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
    /// `fogar_ports::ipc::SEND_TEXT_MAX_BYTES`.
    static let textoMaxBytes = 8 * 1024

    let link: ConversationLink
    /// The text box.
    var texto = ""
    private(set) var toma: Toma = .ninguna
    /// One sentence about the last thing that went wrong (cleared by the next action).
    private(set) var aviso: String?
    private(set) var enviando = false
    /// The system's microphone prompt may be open: no second request meanwhile.
    private(set) var pidiendoPermiso = false
    /// Whether the conversation is on screen (section selected, window visible): only then is a
    /// push confirmed, so only then can it sound.
    var aLaVista = false {
        didSet { if aLaVista, !oldValue, let v = link.vista { confirmar(v) } }
    }
    /// The draft last copied into the box, so the same draft is not copied twice.
    @ObservationIgnored private var ultimoBorrador: String?
    /// Turn ids the agent noted as visible (`turn_visible_noted`), bounded; and those in flight.
    @ObservationIgnored private var confirmados: [String] = []
    @ObservationIgnored private var confirmando: Set<String> = []
    /// Bumped when the draft lands in the box (the view focuses the field).
    private(set) var borradorNuevo = 0
    /// Every task this model starts (forgotten when done), so none is left without an owner.
    @ObservationIgnored private var tareas: [UInt64: Task<Void, Never>] = [:]
    @ObservationIgnored private var siguienteTarea: UInt64 = 0

    init(link: ConversationLink) {
        self.link = link
        link.alCambiar = { [weak self] v in self?.actualizar(v) }
        link.alPerder = { [weak self] in self?.perdido() }
    }

    var vista: WireConversation? { link.vista }

    // MARK: Derived state

    /// The conversation connection is up (otherwise the view on screen is the last one seen).
    var vivo: Bool { link.enlace == .viva }

    /// The agent's microphone is open (its own word, never this app's guess).
    var grabando: Bool { vivo && vista?.activity == .recording }

    /// A turn is open, a reply is awaited or audio plays: «Cancelar» makes sense.
    var cancelable: Bool {
        guard vivo, let v = vista else { return false }
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
        case .unknown: return .desconocido
        case .granted: break
        }
        return v.transcriber == .down ? .sinTranscriptor : .listo
    }

    /// Text that may be sent now (trimmed, within the agent's limit, no take open).
    var puedeEnviar: Bool {
        let t = texto.trimmingCharacters(in: .whitespacesAndNewlines)
        return !t.isEmpty && vivo && !grabando && toma == .ninguna && !enviando
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
        // After `capture_started`, any view without `recording` means the take is over (cut by the
        // agent, or ended): the agent pushes the newest view only, never an older one.
        if v.activity != .recording {
            switch toma {
            case .abierta, .cerrando: toma = .ninguna
            default: break
            }
        }
        confirmar(v)
    }

    /// `turn_visible` for what is on screen: my turns always (I just wrote or spoke, and the menu
    /// bar says «hablando»), a push only while the conversation is in view. A turn counts as
    /// confirmed only once the agent noted it; a failed confirmation is retried on the next view.
    private func confirmar(_ v: WireConversation) {
        for id in Self.porConfirmar(v, aLaVista: aLaVista, ya: Set(confirmados).union(confirmando)) {
            confirmando.insert(id)
            lanzar { [weak self] in
                guard let self else { return }
                let r = try? await self.link.pedir(.turnVisible(WireTurnVisible(turn_id: id)))
                self.confirmando.remove(id)
                guard case .turnVisibleNoted? = r else { return }
                self.confirmados.append(id)
                if self.confirmados.count > 64 { self.confirmados.removeFirst(self.confirmados.count - 64) }
            }
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

    /// The conversation connection dropped: the agent ended any take with it (`discard`), and a
    /// new subscription starts its confirmations from scratch.
    private func perdido() {
        toma = .ninguna
        confirmados.removeAll()
        confirmando.removeAll()
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
            case .refused(let r): aviso = Textos.rechazo(r.reason)
            default: aviso = Textos.fallo(CommandError.unexpected)
            }
        } catch {
            aviso = Textos.fallo(error)
        }
    }

    func cancelar() async {
        aviso = nil
        do {
            switch try await link.pedir(.cancelTurn) {
            case .turnCancelSent: break
            case .refused(let r): aviso = Textos.rechazo(r.reason)
            default: aviso = Textos.fallo(CommandError.unexpected)
            }
        } catch {
            aviso = Textos.fallo(error)
        }
    }

    // MARK: The take

    /// The pointer went down on the microphone button: the take opens at once (no speech lost);
    /// the release decides whether it was a click or a hold. On an open clicked take, the button
    /// stops it to review.
    func pulsar(ahora: Date = Date()) {
        switch toma {
        case .ninguna where !grabando: empezar(ahora: ahora)
        case .abierta(false, _, false): terminar(.review)
        default: break
        }
    }

    /// The pointer came up (or the gesture was cancelled). Held past the threshold: send. A click
    /// leaves the take open.
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

    /// Called while the pointer is still down: past the threshold the take shows «suelta para
    /// enviar». Only a live press is promoted (the release clears `pulsado`).
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

    /// The conversation left the screen with a take open: never keep listening unseen, and never
    /// send what nobody reviewed. The transcript goes to the box.
    func vistaDesaparece() {
        switch toma {
        case .abriendo, .abierta: terminar(.review)
        default: break
        }
    }

    private func empezar(ahora: Date, pulsado: Bool = true) {
        aviso = nil
        toma = .abriendo(desde: ahora, pulsado: pulsado, fin: nil)
        lanzar { [weak self] in
            guard let self else { return }
            do {
                // GUESSED: the gesture only feeds the agent's metrics and is not known before the
                // release (the take opens on press so no speech is lost): reported `toggle`.
                let r = try await self.link.pedir(.captureStart(WireCaptureStart(gesture: .toggle)))
                self.alAbrir(r)
            } catch {
                self.toma = .ninguna
                self.aviso = Textos.fallo(error)
            }
        }
    }

    /// The answer to `capture_start`.
    private func alAbrir(_ r: WireResponse) {
        switch r {
        case .captureStarted:
            // Not opening any more: the link dropped meanwhile (the agent discarded it with it).
            guard case .abriendo(let desde, let pulsado, let fin) = toma else { return }
            if let fin {
                toma = .cerrando
                enviarFin(fin)
            } else {
                toma = .abierta(mantener: false, desde: desde, pulsado: pulsado)
                comprobarMantener()
            }
        case .captureRefused(let c):
            toma = .ninguna
            aviso = Textos.rechazoToma(c.reason)
        case .refused(let c):
            toma = .ninguna
            aviso = Textos.rechazo(c.reason)
        default:
            toma = .ninguna
            aviso = Textos.fallo(CommandError.unexpected)
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
        lanzar { [weak self] in
            guard let self else { return }
            do {
                switch try await self.link.pedir(.captureEnd(WireCaptureEnd(then: then))) {
                case .captureEnded: break
                case .refused(let r): self.aviso = Textos.rechazo(r.reason)
                default: self.aviso = Textos.fallo(CommandError.unexpected)
                }
            } catch {
                // The connection dropped: the agent already discarded the take with it. Only say
                // so when the person wanted to keep it.
                if then != .discard { self.aviso = Textos.tomaPerdida }
            }
            if self.toma == .cerrando { self.toma = .ninguna }
        }
    }

    // MARK: Permission

    func pedirPermiso() async {
        guard !pidiendoPermiso else { return }
        pidiendoPermiso = true
        aviso = nil
        defer { pidiendoPermiso = false }
        do {
            // Its own connection: the system prompt can take as long as the person needs, and a
            // Cancel on the conversation never waits behind it.
            switch try await link.pedirAparte(.requestMicPermission, plazo: .seconds(120)) {
            case .micPermission(let p) where p.state == .denied || p.state == .restricted:
                aviso = Textos.microDenegado
            case .micPermission: break
            case .refused(let r): aviso = Textos.rechazo(r.reason)
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

    // MARK: Tasks

    /// Starts `op` as a task this model keeps until it is done.
    private func lanzar(_ op: @escaping @MainActor () async -> Void) {
        siguienteTarea += 1
        let id = siguienteTarea
        tareas[id] = Task { [weak self] in
            await op()
            self?.tareas[id] = nil
        }
    }
}
