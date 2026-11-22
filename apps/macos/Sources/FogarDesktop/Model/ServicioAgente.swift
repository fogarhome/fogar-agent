import AppKit
import Foundation
import Observation
import ServiceManagement

/// The agent's LaunchAgent as macOS sees it (`SMAppService.agent`), in the app's own words.
enum EstadoServicio: Equatable, Sendable {
    /// Registered and allowed: launchd starts the agent at login and restarts it if it fails.
    case activo
    /// Registered, but the person has to allow it in System Settings → Login Items.
    case pendienteAprobacion
    /// Not registered (never, or the person removed it): the agent does not start by itself.
    case desactivado
    /// macOS cannot find the agent's plist in this app (a damaged or partial copy).
    case noEncontrado
    /// This build carries no agent (debug builds: they talk to the stand-in or a hand-run agent).
    case noIncluido
}

/// What the app needs from the system's registry of login items. A seam for tests: the real one
/// is `RegistroSMAppService`. Called off the main thread (the latency of `SMAppService` is not
/// documented; fogar-macos-daemon hard rule 14).
nonisolated protocol RegistroAgente: Sendable {
    var estado: EstadoServicio { get }
    func registrar() throws
    func quitar() throws
}

/// `SMAppService.agent(plistName:)` for `Contents/Library/LaunchAgents/com.fogar.agent.plist`.
nonisolated struct RegistroSMAppService: RegistroAgente {
    /// Frozen with the plist's label (fogar-macos-daemon hard rule 2).
    static let plist = "com.fogar.agent.plist"
    /// The plist is inside this app (only release builds carry the agent).
    let incluido: Bool

    init(bundle: Bundle = .main) {
        let ruta = bundle.bundleURL.appendingPathComponent("Contents/Library/LaunchAgents/\(Self.plist)").path
        incluido = FileManager.default.fileExists(atPath: ruta)
    }

    private var servicio: SMAppService { .agent(plistName: Self.plist) }

    var estado: EstadoServicio {
        guard incluido else { return .noIncluido }
        return Self.estado(servicio.status)
    }

    static func estado(_ s: SMAppService.Status) -> EstadoServicio {
        switch s {
        case .enabled: .activo
        case .requiresApproval: .pendienteAprobacion
        case .notRegistered: .desactivado
        case .notFound: .noEncontrado
        @unknown default: .noEncontrado
        }
    }

    func registrar() throws { try servicio.register() }
    func quitar() throws { try servicio.unregister() }
}

/// The agent's registration, shown in Ajustes. The app registers the agent by itself once, the
/// first time it runs (spec S2 E2); after that only the person changes it, here or in System
/// Settings. There is no change notification, so the state is read again whenever the app becomes
/// active (hard rule 14), never on a timer.
@Observable
final class ServicioAgente {
    private(set) var estado: EstadoServicio?
    private(set) var trabajando = false
    private(set) var error: String?

    @ObservationIgnored private let registro: any RegistroAgente
    @ObservationIgnored private let defaults: UserDefaults
    @ObservationIgnored private var observador: (any NSObjectProtocol)?
    /// Set once the first automatic registration has been tried, whatever its outcome: removing
    /// the agent here or in System Settings is then the person's decision, not undone at launch.
    static let claveRegistroInicial = "agente.registroInicialIntentado"

    init(registro: any RegistroAgente = RegistroSMAppService(), defaults: UserDefaults = .standard) {
        self.registro = registro
        self.defaults = defaults
    }

    /// At launch: the first registration if it never happened, then the state; and from now on the
    /// state again on every activation.
    func arrancar() async {
        if observador == nil {
            observador = NotificationCenter.default.addObserver(
                forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main
            ) { [weak self] _ in
                MainActor.assumeIsolated { _ = Task { await self?.refrescar() } }
            }
        }
        let actual = await leer()
        if actual == .desactivado, !defaults.bool(forKey: Self.claveRegistroInicial) {
            defaults.set(true, forKey: Self.claveRegistroInicial)
            await registrar()
        } else {
            if actual != .noIncluido { defaults.set(true, forKey: Self.claveRegistroInicial) }
            estado = actual
        }
    }

    func refrescar() async { estado = await leer() }

    func registrar() async { await cambiar { try $0.registrar() } }
    func quitar() async { await cambiar { try $0.quitar() } }

    func abrirItemsDeInicio() { SMAppService.openSystemSettingsLoginItems() }

    private func leer() async -> EstadoServicio {
        let r = registro
        return await Task.detached { r.estado }.value
    }

    private func cambiar(_ accion: @escaping @Sendable (any RegistroAgente) throws -> Void) async {
        guard !trabajando else { return }
        trabajando = true
        error = nil
        let r = registro
        let fallo: String? = await Task.detached {
            do { try accion(r); return nil } catch { return error.localizedDescription }
        }.value
        if let fallo { error = Textos.falloServicio(fallo) }
        estado = await leer()
        trabajando = false
    }
}

extension Textos {
    static func servicioAgente(_ e: EstadoServicio) -> String {
        switch e {
        case .activo: "El agente arranca solo al iniciar sesión y macOS lo vuelve a abrir si se cae."
        case .pendienteAprobacion: "El agente está registrado, pero falta tu permiso en Ajustes del Sistema → General → Ítems de inicio."
        case .desactivado: "El agente no está registrado: no arrancará solo al iniciar sesión."
        case .noEncontrado: "macOS no encuentra el agente dentro de esta app. Vuelve a copiar Fogar Desktop en Aplicaciones."
        case .noIncluido: "Esta compilación no lleva el agente dentro: habla con el que arranques a mano."
        }
    }

    static func falloServicio(_ detalle: String) -> String {
        "macOS no ha dejado cambiar el registro del agente: \(detalle)"
    }
}
