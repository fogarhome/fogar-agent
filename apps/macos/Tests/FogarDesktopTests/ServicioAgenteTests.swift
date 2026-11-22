import Foundation
import ServiceManagement
import Testing
@testable import FogarDesktop

/// A login-item registry in memory: what `SMAppService` would answer, and what the app asked.
nonisolated final class FakeRegistro: RegistroAgente, @unchecked Sendable {
    private let lock = NSLock()
    private var _estado: EstadoServicio
    private var _registros = 0
    private var _quitados = 0
    /// What `register()` leaves behind (macOS may ask for approval instead of enabling).
    let trasRegistrar: EstadoServicio
    let falla: Bool

    init(_ estado: EstadoServicio, trasRegistrar: EstadoServicio = .activo, falla: Bool = false) {
        _estado = estado
        self.trasRegistrar = trasRegistrar
        self.falla = falla
    }

    var estado: EstadoServicio { lock.withLock { _estado } }
    var registros: Int { lock.withLock { _registros } }
    var quitados: Int { lock.withLock { _quitados } }

    func registrar() throws {
        try lock.withLock {
            _registros += 1
            if falla { throw CocoaError(.featureUnsupported) }
            _estado = trasRegistrar
        }
    }

    func quitar() throws {
        lock.withLock {
            _quitados += 1
            _estado = .desactivado
        }
    }
}

@Suite struct ServicioAgenteTests {
    private func defaults() -> UserDefaults {
        let nombre = "fogar-test-\(UUID().uuidString)"
        let d = UserDefaults(suiteName: nombre)!
        d.removePersistentDomain(forName: nombre)
        return d
    }

    @Test func primeraVezRegistraUnaSolaVez() async {
        let d = defaults()
        let r = FakeRegistro(.desactivado)
        let s = ServicioAgente(registro: r, defaults: d)
        await s.arrancar()
        #expect(r.registros == 1)
        #expect(s.estado == .activo)
        // The person removes it: the next launch does not register it again.
        await s.quitar()
        #expect(s.estado == .desactivado)
        let otra = ServicioAgente(registro: r, defaults: d)
        await otra.arrancar()
        #expect(r.registros == 1)
        #expect(otra.estado == .desactivado)
    }

    @Test func pendienteDeAprobacionSeMuestraSinInsistir() async {
        let r = FakeRegistro(.desactivado, trasRegistrar: .pendienteAprobacion)
        let s = ServicioAgente(registro: r, defaults: defaults())
        await s.arrancar()
        #expect(s.estado == .pendienteAprobacion)
        await s.refrescar()
        #expect(r.registros == 1)
    }

    @Test func yaRegistradoNoSeVuelveARegistrar() async {
        let r = FakeRegistro(.activo)
        let s = ServicioAgente(registro: r, defaults: defaults())
        await s.arrancar()
        #expect(r.registros == 0)
        #expect(s.estado == .activo)
    }

    /// A debug build carries no agent: nothing is registered, and a later release build still
    /// gets its first registration.
    @Test func sinAgenteDentroNoRegistraNada() async {
        let d = defaults()
        let r = FakeRegistro(.noIncluido)
        let s = ServicioAgente(registro: r, defaults: d)
        await s.arrancar()
        #expect(r.registros == 0)
        #expect(s.estado == .noIncluido)
        #expect(!d.bool(forKey: ServicioAgente.claveRegistroInicial))
    }

    @Test func unFalloSeCuentaYElEstadoSeRelee() async {
        let r = FakeRegistro(.desactivado, falla: true)
        let s = ServicioAgente(registro: r, defaults: defaults())
        await s.arrancar()
        #expect(r.registros == 1)
        #expect(s.estado == .desactivado)
        #expect(s.error?.hasPrefix("macOS no ha dejado cambiar el registro del agente") == true)
        #expect(!s.trabajando)
    }

    @Test func estadosDeSMAppService() {
        #expect(RegistroSMAppService.estado(.enabled) == .activo)
        #expect(RegistroSMAppService.estado(.requiresApproval) == .pendienteAprobacion)
        #expect(RegistroSMAppService.estado(.notRegistered) == .desactivado)
        #expect(RegistroSMAppService.estado(.notFound) == .noEncontrado)
        // The test runner is no app bundle with the agent inside.
        #expect(RegistroSMAppService().estado == .noIncluido)
    }

    @Test func textos() {
        for e in [EstadoServicio.activo, .pendienteAprobacion, .desactivado, .noEncontrado, .noIncluido] {
            #expect(!Textos.servicioAgente(e).isEmpty)
        }
        #expect(Textos.servicioAgente(.pendienteAprobacion).contains("Ítems de inicio"))
    }
}
