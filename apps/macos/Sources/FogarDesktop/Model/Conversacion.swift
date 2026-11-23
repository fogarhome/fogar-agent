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
/// only remembers what the person asked for.
enum Toma: Equatable {
    case ninguna
    /// `capture_start` sent, no answer yet. `fin`: how to end it as soon as the agent says it
    /// opened (send, stop or Esc pressed meanwhile).
    case abriendo(fin: WireThen?)
    /// Open: the box shows the live transcript and the two ways out (send, stop).
    case abierta
    /// `capture_end` sent.
    case cerrando
}

/// A key the app handles itself, before any view or menu: SwiftUI shortcuts on in-window buttons
/// depend on that button being in the key window's live view tree (#33).
nonisolated enum Tecla: Equatable, Sendable {
    /// ⌘. — cancel Fogar's reply.
    case cancelar
    /// Esc — discard the open take.
    case escape
    /// Return — send the open take.
    case intro

    /// The plain ⌘., Esc or Return of a key-down event (no other modifier, no repeat).
    static func de(_ e: NSEvent) -> Tecla? {
        guard e.type == .keyDown, !e.isARepeat else { return nil }
        let mods = e.modifierFlags.intersection([.command, .option, .control, .shift])
        switch mods {
        case [.command] where e.charactersIgnoringModifiers == ".": return .cancelar
        case [] where e.keyCode == 53: return .escape // kVK_Escape
        case [] where e.keyCode == 36 || e.keyCode == 76: return .intro // kVK_Return, kVK_ANSI_KeypadEnter
        default: return nil
        }
    }
}

/// The conversation section's model: the text box, the take, the visible-turn
/// confirmations and the one-line feedback. Everything it shows comes from the agent's view.
@Observable
final class Conversacion {
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
    /// Refused for good (`unsupported request`, not the signed app…): never asked again.
    @ObservationIgnored private var descartados: [String] = []
    /// Transient refusals (`engine busy`) so far, per turn.
    @ObservationIgnored private var intentos: [String: Int] = [:]
    /// At most this many `turn_visible` per turn when the agent is busy, 0.5 s, 1 s, 2 s apart.
    nonisolated static let maxIntentosVisible = 3
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
        // agent, or ended): the agent pushes the newest view only, never an older one. This relies
        // on the core publishing the `recording` view BEFORE it answers `capture_started` (asked
        // of S2 3b in the orchestrator's issue on that order); until then a stale `idle` view
        // right after the answer would end the take here.
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
        let ya = Set(confirmados).union(confirmando).union(descartados)
        for id in Self.porConfirmar(v, aLaVista: aLaVista, ya: ya) {
            confirmando.insert(id)
            lanzar { [weak self] in
                guard let self else { return }
                let r = try? await self.link.pedir(.turnVisible(WireTurnVisible(turn_id: id)))
                switch Self.tras(r, intentos: self.intentos[id, default: 0]) {
                case .anotado:
                    self.confirmando.remove(id)
                    self.anotar(id)
                case .descartar:
                    self.confirmando.remove(id)
                    self.descartar(id)
                case .reintentar(let espera):
                    // Still «in flight» while waiting, so new views do not ask again meanwhile.
                    self.intentos[id, default: 0] += 1
                    try? await Task.sleep(for: espera)
                    guard self.confirmando.remove(id) != nil else { return } // reset by a reconnection
                    if let v = self.link.vista { self.confirmar(v) }
                case .siguienteVista:
                    self.confirmando.remove(id) // the connection dropped: the new one starts over
                }
            }
        }
    }

    enum TrasConfirmar: Equatable {
        case anotado, descartar, siguienteVista
        case reintentar(Duration)
    }

    /// What to do after a `turn_visible` answer (`nil`: no answer, the connection dropped).
    static func tras(_ r: WireResponse?, intentos: Int) -> TrasConfirmar {
        switch r {
        case .turnVisibleNoted?: return .anotado
        case nil: return .siguienteVista
        case .refused(let x)? where x.reason == "engine busy":
            guard intentos + 1 < maxIntentosVisible else { return .descartar }
            return .reintentar(.milliseconds(500 << intentos))
        default: return .descartar // not transient: another answer will not change it
        }
    }

    private func anotar(_ id: String) {
        intentos[id] = nil
        confirmados.append(id)
        if confirmados.count > 64 { confirmados.removeFirst(confirmados.count - 64) }
    }

    private func descartar(_ id: String) {
        intentos[id] = nil
        descartados.append(id)
        if descartados.count > 64 { descartados.removeFirst(descartados.count - 64) }
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
        descartados.removeAll()
        intentos.removeAll()
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

    /// The microphone button (click, Space or VoiceOver): start a take. Like Claude's box, the take
    /// then ends with one of its two buttons: send, or stop and leave the text in the box.
    func grabar() {
        guard toma == .ninguna, !grabando else { return }
        aviso = nil
        toma = .abriendo(fin: nil)
        lanzar { [weak self] in
            guard let self else { return }
            do {
                let r = try await self.link.pedir(.captureStart(WireCaptureStart(gesture: .toggle)))
                self.alAbrir(r)
            } catch {
                self.toma = .ninguna
                self.aviso = Textos.fallo(error)
            }
        }
    }

    /// Stop: the transcript stays in the box to edit or send (`review`).
    func parar() { terminar(.review) }
    /// Send the take as it is (`send`).
    func enviarToma() { terminar(.send) }
    /// Esc: throw the take away (`discard`).
    func descartar() { terminar(.discard) }

    /// The person's hands are in a take (opening, open or closing) or the agent records.
    var enToma: Bool { grabando || toma != .ninguna }

    /// The conversation left the screen with a take open: never keep listening unseen, and never
    /// send what nobody reviewed. The transcript goes to the box.
    func vistaDesaparece() {
        switch toma {
        case .abriendo, .abierta: terminar(.review)
        default: break
        }
    }

    /// The answer to `capture_start`.
    private func alAbrir(_ r: WireResponse) {
        switch r {
        case .captureStarted:
            // Not opening any more: the link dropped meanwhile (the agent discarded it with it).
            guard case .abriendo(let fin) = toma else { return }
            if let fin {
                toma = .cerrando
                enviarFin(fin)
            } else {
                toma = .abierta
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
        case .abriendo(fin: .none):
            toma = .abriendo(fin: then)
        case .abriendo:
            break // the first way out wins, as with an open take
        case .ninguna where grabando:
            enviarFin(then)
        default:
            break
        }
    }

    // MARK: Keys

    /// A key the app saw before any view: `true` if it was used (the event then goes no further).
    /// ⌘. cancels wherever the app has focus (window, menu bar panel, any field) when there is
    /// something to cancel. Esc and Return only end a take while the conversation is on screen.
    /// `enLaConversacion`: the event belongs to the conversation's window, which is key, with no
    /// sheet and no text being composed (IME) (`admiteTeclasDeToma`).
    func tecla(_ t: Tecla, enLaConversacion: Bool = true) -> Bool {
        let tomaViva = aLaVista && enLaConversacion && enToma && toma != .cerrando
        switch t {
        case .cancelar:
            guard cancelable else { return false }
            cancelarAhora()
            return true
        case .escape where tomaViva:
            descartar()
            return true
        case .intro where tomaViva:
            enviarToma()
            return true
        default:
            return false
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

    // MARK: From the views (the task belongs to this model, not to a view that may go away)

    func enviarAhora() { lanzar { [weak self] in await self?.enviar() } }
    func cancelarAhora() { lanzar { [weak self] in await self?.cancelar() } }
    func pedirPermisoAhora() { lanzar { [weak self] in await self?.pedirPermiso() } }

    // MARK: The keyboard

    /// Installed once (idempotent), removed with the model: every key-down of every window of
    /// the app (main window, menu bar panel, Settings) passes here first, whatever has focus.
    /// Written only on the main actor; read in `deinit`, when nothing else holds the model.
    @ObservationIgnored nonisolated(unsafe) private(set) var monitor: Any?
    /// The conversation's window, set by the view (Esc and Return only count there).
    @ObservationIgnored weak var ventana: NSWindow?

    func escucharTeclado() {
        guard monitor == nil else { return }
        // AppKit calls this on the main thread, as it dispatches the event.
        monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] e in
            guard let t = Tecla.de(e) else { return e }
            let n = e.windowNumber
            let usada = MainActor.assumeIsolated {
                guard let self else { return false }
                return self.tecla(t, enLaConversacion: self.admiteTeclasDeToma(ventanaDelEvento: n))
            }
            return usada ? nil : e
        }
    }

    deinit {
        if let monitor { NSEvent.removeMonitor(monitor) }
    }

    /// Esc and Return end a take only from the conversation's window, while it is the key window,
    /// has no sheet open and no field is composing text (IME): never from the menu bar panel,
    /// Settings or a dialog.
    func admiteTeclasDeToma(ventanaDelEvento n: Int) -> Bool {
        guard let w = ventana, w.windowNumber == n, w.isKeyWindow, w.attachedSheet == nil else { return false }
        if let campo = w.firstResponder as? NSTextView, campo.hasMarkedText() { return false }
        return true
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
