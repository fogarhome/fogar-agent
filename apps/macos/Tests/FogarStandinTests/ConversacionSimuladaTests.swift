import FogarStandinKit
import FogarWire
import Testing

/// The stand-in plays the core's conversation rules (IPC v4); the app is developed against it, so
/// its rules are checked here: the consent point, the output point and the bounded view.
@Suite struct ConversacionSimuladaTests {
    @Test func textoValidadoComoElNucleo() {
        var c = ConversacionSimulada(mic: .granted)
        #expect(c.enviarTexto("  \n") == .textRefused(WireTextRefused(reason: .empty)))
        #expect(c.enviarTexto(String(repeating: "a", count: LimitesConversacion.textoMaxBytes + 1))
            == .textRefused(WireTextRefused(reason: .tooLong)))
        c.listo = false
        #expect(c.enviarTexto("hola") == .textRefused(WireTextRefused(reason: .notReady)))
        c.listo = true
        #expect(c.empezarToma(conexion: 1, suscrita: true) == .captureStarted)
        #expect(c.enviarTexto("hola") == .textRefused(WireTextRefused(reason: .recording)))
        _ = c.terminarToma(.discard)
        #expect(c.enviarTexto("hola") == .textAccepted)
        #expect(c.activity == .awaitingReply)
    }

    @Test func laTomaPasaPorElPuntoDeConsentimiento() {
        var c = ConversacionSimulada(mic: .notDetermined)
        #expect(c.empezarToma(conexion: 1, suscrita: false) == .captureRefused(WireCaptureRefused(reason: .notSubscribed)))
        #expect(c.empezarToma(conexion: 1, suscrita: true) == .captureRefused(WireCaptureRefused(reason: .permissionNotDetermined)))
        #expect(c.activity == .idle)
        #expect(c.pedirPermiso(concede: false) == .micPermission(WireMicPermission(state: .denied)))
        #expect(c.empezarToma(conexion: 1, suscrita: true) == .captureRefused(WireCaptureRefused(reason: .permissionDenied)))
        c.mic = .granted
        c.transcriber = .down
        #expect(c.empezarToma(conexion: 1, suscrita: true) == .captureRefused(WireCaptureRefused(reason: .noTranscriber)))
        c.transcriber = .up
        #expect(c.empezarToma(conexion: 1, suscrita: true) == .captureStarted)
        #expect(c.empezarToma(conexion: 2, suscrita: true) == .captureRefused(WireCaptureRefused(reason: .busy)))
    }

    @Test func revisarDejaElTextoEnLaCajaYEnviarAbreTurno() {
        var c = ConversacionSimulada(mic: .granted)
        _ = c.empezarToma(conexion: 1, suscrita: true)
        c.parcial("enciende la luz")
        #expect(c.vista.live == "enciende la luz")
        let (r, enviar) = c.terminarToma(.review)
        #expect(r == .captureEnded && enviar == nil)
        #expect(c.vista.draft == "enciende la luz" && c.vista.live.isEmpty && c.activity == .idle)
        // Ending twice is fine (idempotent), and sending clears the draft.
        #expect(c.terminarToma(.send).respuesta == .captureEnded)
        _ = c.empezarToma(conexion: 1, suscrita: true)
        c.parcial("apaga la tele")
        #expect(c.terminarToma(.send).enviar == "apaga la tele")
        #expect(c.activity == .awaitingReply)
        let id = c.abrirTurno(source: .mine, mine: "apaga la tele")
        #expect(c.activity == .idle && c.turno(id)?.progress == .preparing)
    }

    @Test func uncorteAutomaticoNuncaEnvia() {
        var c = ConversacionSimulada(mic: .granted)
        _ = c.empezarToma(conexion: 1, suscrita: true)
        c.parcial("me quedé pensando")
        c.cortarToma(.silenceCut)
        #expect(c.activity == .idle && c.draft == "me quedé pensando" && c.notice == .silenceCut)
        #expect(c.turns.isEmpty)
    }

    @Test func laTomaEsDeSuConexion() {
        var c = ConversacionSimulada(mic: .granted)
        _ = c.empezarToma(conexion: 7, suscrita: true)
        c.parcial("algo privado")
        c.conexionPerdida(3)
        #expect(c.activity == .recording)
        c.conexionPerdida(7)
        #expect(c.activity == .idle && c.draft == nil && c.live.isEmpty)
    }

    @Test func nadaSuenaSinTurnVisible() {
        var c = ConversacionSimulada(mic: .granted)
        let id = c.abrirTurno(source: .push, mine: "")
        c.texto(id, "Recuerda la lavadora")
        c.llegaAudio(id)
        #expect(!c.speaking && c.vista.awaiting_visible == id)
        c.plazoVencido(id)
        #expect(!c.speaking && c.vista.awaiting_visible == nil && c.turno(id)?.audio == .muted && c.audiosTirados == 1)
        // A late confirmation does not bring the dropped audio back.
        _ = c.turnoVisible(id)
        #expect(!c.speaking)
    }

    @Test func conTurnVisibleSuenaYCancelarLoCalla() {
        var c = ConversacionSimulada(mic: .granted)
        let id = c.abrirTurno(source: .mine, mine: "hola")
        #expect(c.turnoVisible(id) == .turnVisibleNoted)
        c.texto(id, "Hola")
        #expect(c.turno(id)?.progress == nil)
        c.llegaAudio(id)
        #expect(c.speaking && c.vista.awaiting_visible == nil)
        #expect(c.cancelar() == .turnCancelSent)
        #expect(!c.speaking && c.turno(id)?.outcome == .cancelled)
        // A turn that ends while its audio waits is text only.
        let otro = c.abrirTurno(source: .push, mine: "")
        c.llegaAudio(otro)
        c.cerrar(otro, .done)
        #expect(c.turno(otro)?.audio == .muted && c.vista.awaiting_visible == nil)
    }

    @Test func laVistaEstaAcotada() {
        var c = ConversacionSimulada(mic: .granted)
        for i in 0..<(LimitesConversacion.turnosMax + 5) {
            let id = c.abrirTurno(source: .mine, mine: "pregunta \(i)")
            c.cerrar(id, .done)
        }
        #expect(c.turns.count == LimitesConversacion.turnosMax)
        #expect(c.turns.first?.mine == "pregunta 5")
        let largo = String(repeating: "x", count: 30 * 1024)
        for _ in 0..<3 {
            let id = c.abrirTurno(source: .mine, mine: largo)
            c.cerrar(id, .done)
        }
        #expect(c.turns.reduce(0) { $0 + $1.mine.utf8.count + $1.reply.utf8.count } <= LimitesConversacion.textoVistaMaxBytes)
    }

    @Test func laVistaViajaPorElCable() throws {
        var c = ConversacionSimulada(mic: .granted)
        let id = c.abrirTurno(source: .mine, mine: "¿Qué tiempo hace?")
        c.progreso(id, .tool, herramienta: "ha_call_service")
        let ida = try WireCodec.encode(WireResponse.conversation(c.vista))
        #expect(try WireCodec.decodeResponse(ida) == .conversation(c.vista))
        #expect(ida.count < maxFrameBytes)
    }
}
