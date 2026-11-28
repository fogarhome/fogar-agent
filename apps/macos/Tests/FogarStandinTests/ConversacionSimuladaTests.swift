import FogarStandinKit
import FogarWire
import Testing

/// The stand-in plays the core's conversation rules (IPC v6); the app is developed against it, so
/// its rules are checked here: the consent point, the list, pending messages and the bounded view.
@Suite struct ConversacionSimuladaTests {
    @Test func textoValidadoComoElNucleo() {
        var c = ConversacionSimulada(mic: .granted, transcriber: .up)
        #expect(c.enviarTexto("  \n").respuesta == .textRefused(WireTextRefused(reason: .empty)))
        #expect(c.enviarTexto(String(repeating: "a", count: LimitesConversacion.textoMaxBytes + 1)).respuesta
            == .textRefused(WireTextRefused(reason: .tooLong)))
        c.listo = false
        #expect(c.enviarTexto("hola").respuesta == .textRefused(WireTextRefused(reason: .notReady)))
        c.listo = true
        c.soloLectura = true
        #expect(c.enviarTexto("hola").respuesta == .textRefused(WireTextRefused(reason: .missingScope)))
        c.soloLectura = false
        #expect(c.empezarToma(conexion: 1, suscrita: true) == .captureStarted)
        #expect(c.enviarTexto("hola").respuesta == .textRefused(WireTextRefused(reason: .recording)))
        _ = c.terminarToma(.discard, conexion: 1)
        #expect(c.enviarTexto("hola").respuesta == .textAccepted)
    }

    @Test func sinNadaAbiertoElTextoCreaUnaConversacion() {
        var c = ConversacionSimulada(ejemplos: true)
        #expect(c.lista.count == 2 && c.abierta == nil && c.vista.id == nil)
        let r = c.enviarTexto("¿Qué hay para cenar?")
        #expect(r.respuesta == .textAccepted && r.entregado)
        #expect(c.abierta == r.en && c.lista.first?.id == r.en && c.lista.count == 3)
        guard case .message(let m)? = c.entries.last else { Issue.record("no message"); return }
        #expect(m.mine && m.state == .delivered && m.text == "¿Qué hay para cenar?")
        #expect(c.nueva() == .conversationOpened && c.vista.id == nil && c.entries.isEmpty)
        #expect(c.abrir("conv-serie") == .conversationOpened && c.entries.count == 2)
        #expect(c.abrir("no-existe") == .refused(WireRefused(reason: "unknown conversation")))
    }

    @Test func pendienteMientrasCorreUnTurnoYEnMarchaArriba() {
        var c = ConversacionSimulada(ejemplos: true)
        _ = c.abrir("conv-compra")
        #expect(c.lista.first?.id == "conv-serie")
        let t = c.abrirTurno(source: .mine, en: "conv-compra")
        #expect(c.lista.first?.id == "conv-compra" && c.lista.first?.running == true)
        let r = c.enviarTexto("y pan")
        #expect(!r.entregado)
        guard case .message(let m)? = c.entries.last else { Issue.record("no message"); return }
        #expect(m.state == .pending)
        #expect(c.cerrar(t, .done) == "y pan")
        #expect(c.lista.first?.running == false)
        guard case .message(let d)? = c.entries.last else { Issue.record("no message"); return }
        #expect(d.state == .delivered)
    }

    @Test func cancelarRetiraLoPendiente() {
        var c = ConversacionSimulada(ejemplos: true)
        _ = c.abrir("conv-serie")
        let t = c.abrirTurno(source: .mine, en: "conv-serie")
        _ = c.enviarTexto("me arrepiento")
        c.listo = false
        #expect(c.cancelar() == .turnCancelNotReady)
        c.listo = true
        #expect(c.cancelar() == .turnCancelSent)
        #expect(c.turno(t)?.outcome == .cancelled)
        #expect(!c.entries.contains { if case .message(let m) = $0 { m.text == "me arrepiento" } else { false } })
    }

    @Test func otroDispositivoConSuNombre() {
        var c = ConversacionSimulada(ejemplos: true)
        _ = c.abrir("conv-serie")
        c.mensaje("¿Qué tiempo hace?", en: "conv-serie", mio: false, de: "Cocina")
        let t = c.abrirTurno(source: .mine, en: "conv-serie", por: "Cocina")
        guard case .message(let m) = c.entries[c.entries.count - 2] else { Issue.record("no message"); return }
        #expect(!m.mine && m.from_name == "Cocina")
        #expect(c.turno(t)?.by_name == "Cocina" && c.turno(t)?.by_self == false)
    }

    @Test func laTomaPasaPorElPuntoDeConsentimiento() {
        var c = ConversacionSimulada(mic: .notDetermined, transcriber: .up)
        #expect(c.empezarToma(conexion: 1, suscrita: false) == .captureRefused(WireCaptureRefused(reason: .notSubscribed)))
        #expect(c.empezarToma(conexion: 1, suscrita: true) == .captureRefused(WireCaptureRefused(reason: .permissionNotDetermined)))
        #expect(c.activity == .idle)
        #expect(c.pedirPermiso(concede: false) == .micPermission(WireMicPermission(state: .denied)))
        #expect(c.empezarToma(conexion: 1, suscrita: true) == .captureRefused(WireCaptureRefused(reason: .permissionDenied)))
        c.mic = .granted
        // Dictation is off until PR 5: no transcriber reported `up` refuses every take.
        for t in [WireService.down, .unknown] {
            c.transcriber = t
            #expect(c.empezarToma(conexion: 1, suscrita: true) == .captureRefused(WireCaptureRefused(reason: .noTranscriber)))
        }
        c.transcriber = .up
        c.sinMicro = true
        #expect(c.empezarToma(conexion: 1, suscrita: true) == .captureRefused(WireCaptureRefused(reason: .noMicrophone)))
        c.sinMicro = false
        #expect(c.empezarToma(conexion: 1, suscrita: true) == .captureStarted)
        #expect(c.empezarToma(conexion: 2, suscrita: true) == .captureRefused(WireCaptureRefused(reason: .busy)))
    }

    @Test func revisarDejaElTextoEnLaCaja() {
        var c = ConversacionSimulada(mic: .granted, transcriber: .up)
        _ = c.empezarToma(conexion: 1, suscrita: true)
        c.parcial("enciende la luz")
        #expect(c.vista.live == "enciende la luz")
        // Only the take's own connection ends it.
        #expect(c.terminarToma(.review, conexion: 2).respuesta == .captureEnded && c.activity == .recording)
        let (r, enviar) = c.terminarToma(.review, conexion: 1)
        #expect(r == .captureEnded && enviar == nil)
        #expect(c.vista.draft == "enciende la luz" && c.vista.live.isEmpty && c.activity == .idle)
        #expect(c.terminarToma(.send, conexion: 1).respuesta == .captureEnded)
        _ = c.empezarToma(conexion: 1, suscrita: true)
        c.parcial("apaga la tele")
        #expect(c.terminarToma(.send, conexion: 1).enviar == "apaga la tele")
    }

    @Test func uncorteAutomaticoNuncaEnvia() {
        var c = ConversacionSimulada(mic: .granted, transcriber: .up)
        _ = c.empezarToma(conexion: 1, suscrita: true)
        c.parcial("me quedé pensando")
        c.cortarToma(.silenceCut)
        #expect(c.activity == .idle && c.draft == "me quedé pensando" && c.notice == .silenceCut)
        #expect(c.entries.isEmpty)
    }

    @Test func laTomaEsDeSuConexion() {
        var c = ConversacionSimulada(mic: .granted, transcriber: .up)
        _ = c.empezarToma(conexion: 7, suscrita: true)
        c.parcial("algo privado")
        c.conexionPerdida(3)
        #expect(c.activity == .recording)
        c.conexionPerdida(7)
        #expect(c.activity == .idle && c.draft == nil && c.live.isEmpty)
    }

    @Test func laVistaEstaAcotada() {
        var c = ConversacionSimulada()
        _ = c.enviarTexto("primera")
        let conv = c.abierta ?? ""
        for i in 0..<(LimitesConversacion.entradasMax + 5) {
            let id = c.abrirTurno(source: .push, en: conv)
            c.texto(id, "recordatorio \(i)")
            c.cerrar(id, .done)
        }
        #expect(c.entries.count == LimitesConversacion.entradasMax)
        let largo = String(repeating: "x", count: 30 * 1024)
        for _ in 0..<3 {
            let id = c.abrirTurno(source: .push, en: conv)
            c.texto(id, largo)
            c.cerrar(id, .done)
        }
        let bytes = c.entries.reduce(0) { n, e in
            switch e {
            case .message(let m): n + m.text.utf8.count
            case .turn(let t): n + t.reply.utf8.count
            }
        }
        #expect(bytes <= LimitesConversacion.textoVistaMaxBytes)
    }
}
