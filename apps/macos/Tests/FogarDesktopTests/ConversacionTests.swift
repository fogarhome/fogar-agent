import FogarWire
import Foundation
import Testing
@testable import FogarDesktop

nonisolated func turno(_ id: String, _ source: WireSource = .mine, outcome: WireOutcome? = nil, conversacion: String? = "c1") -> WireTurn {
    WireTurn(turn_id: id, conversation: conversacion, source: source, mine: source == .mine ? "hola" : "", reply: "",
             progress: outcome == nil ? .thinking : nil, tool: nil, outcome: outcome, error_code: nil, audio: .none)
}

nonisolated func vista(_ turns: [WireTurn] = [], activity: WireActivity = .idle, live: String = "", draft: String? = nil,
                       speaking: Bool = false, esperando: String? = nil, mic: WireMic = .granted,
                       transcriber: WireService = .up, notice: WireNotice? = nil) -> WireConversation {
    WireConversation(activity: activity, live: live, draft: draft, turns: turns, speaking: speaking,
                     awaiting_visible: esperando, mic: mic, transcriber: transcriber, tts: .up, notice: notice)
}

/// The rule «Fogar never sounds without something visible», on the app's side.
@Suite struct ConfirmarVisible {
    @Test func misTurnosSiempre() {
        let v = vista([turno("t1"), turno("t2", outcome: .done)])
        #expect(Conversacion.porConfirmar(v, aLaVista: false, ya: []) == ["t1"])
    }

    @Test func unPushSoloConLaConversacionDelante() {
        let v = vista([turno("p1", .push)], esperando: "p1")
        #expect(Conversacion.porConfirmar(v, aLaVista: false, ya: []).isEmpty)
        #expect(Conversacion.porConfirmar(v, aLaVista: true, ya: []) == ["p1"])
    }

    @Test func nadaDosVecesNiDesconocido() {
        let v = vista([turno("t1")], esperando: "t1")
        #expect(Conversacion.porConfirmar(v, aLaVista: true, ya: ["t1"]).isEmpty)
        // A turn the view does not hold (dropped by the bound) is never confirmed blindly.
        #expect(Conversacion.porConfirmar(vista([], esperando: "x9"), aLaVista: true, ya: []).isEmpty)
    }

    @Test func cancelarTieneSentido() {
        #expect(!Conversacion.cancelable(vista([turno("t1", outcome: .done)])))
        #expect(Conversacion.cancelable(vista([turno("t1")])))
        #expect(Conversacion.cancelable(vista(activity: .awaitingReply)))
        #expect(Conversacion.cancelable(vista([turno("t1", outcome: .done)], speaking: true)))
    }
}

@Suite struct BotonDelMicro {
    @Test func cadaEstado() {
        let casos: [(EnlaceConversacion, WireConversation?, Bool, EstadoMicro)] = [
            (.buscando, nil, true, .sinConexion),
            (.sinAgente, nil, true, .sinConexion),
            (.noDisponible, vista(), true, .sinConexion),
            (.soloAppFirmada, nil, true, .soloAppFirmada),
            (.viva, vista(), false, .sinConexion),
            (.viva, vista(mic: .notDetermined), true, .pedirPermiso),
            (.viva, vista(mic: .denied), true, .denegado),
            (.viva, vista(mic: .restricted), true, .denegado),
            (.viva, vista(mic: .unknown), true, .desconocido),
            (.viva, vista(transcriber: .down), true, .sinTranscriptor),
            (.viva, vista(transcriber: .unknown), true, .listo),
            (.viva, vista(), true, .listo),
        ]
        for (enlace, v, listo, esperado) in casos {
            #expect(Conversacion.estadoMicro(enlace: enlace, vista: v, listo: listo) == esperado)
        }
    }

    /// What the gateway names (a tool, an error code) is shown only as a plain identifier.
    @Test func nombresDelGatewayFiltrados() {
        #expect(Textos.progreso(.tool, herramienta: "ha_call_service") == "Usando ha_call_service…")
        #expect(Textos.progreso(.tool, herramienta: "Tu Mac está en peligro, llama al 900") == "Usando una herramienta…")
        #expect(Textos.progreso(.tool, herramienta: String(repeating: "a", count: 65)) == "Usando una herramienta…")
        let raro = WireTurn(turn_id: "t", conversation: nil, source: .mine, mine: "", reply: "", progress: nil, tool: nil,
                            outcome: .error, error_code: "x\ny", audio: .none)
        #expect(Textos.desenlace(raro) == "Algo falló en el gateway.")
    }

    @Test func cadaRechazoDelAgenteConSuFrase() {
        let motivos = ["engine busy", "needs the signed Fogar app", "unsupported request", "ipc version mismatch", "otra cosa"]
        #expect(Set(motivos.map(Textos.rechazo)).count == motivos.count)
    }
}

/// A newer agent must not cost the connection: unknown values read as their closest meaning.
@Suite struct LecturaTolerante {
    @Test func valoresDesconocidosEnLaVista() throws {
        let json = #"{"type":"conversation","data":{"activity":"dancing","live":"","draft":null,"turns":[{"turn_id":"t1","conversation":null,"source":"robot","mine":"","reply":"hola","progress":"juggling","tool":null,"outcome":null,"error_code":null,"audio":"loud"},{"turn_id":"t2","conversation":null,"source":"mine","mine":"a","reply":"b","progress":null,"tool":null,"outcome":"exploded","error_code":null,"audio":"played"}],"speaking":false,"awaiting_visible":null,"mic":"maybe","transcriber":"sleepy","tts":"up","notice":"new_notice"}}"#
        guard case .conversation(let v) = try WireCodec.decodeResponseTolerante(Data(json.utf8)) else {
            Issue.record("not a conversation")
            return
        }
        #expect(v.activity == .awaitingReply && v.mic == .unknown && v.transcriber == .unknown && v.notice == nil)
        #expect(v.turns[0].source == .push && v.turns[0].progress == .working && v.turns[0].audio == WireAudio.none)
        #expect(v.turns[1].outcome == .error)
    }

    @Test func unaRespuestaIlegibleSigueSiendoRespuesta() throws {
        let json = #"{"type":"capture_refused","data":{"reason":"too_sleepy"}}"#
        #expect(try WireCodec.decodeResponseTolerante(Data(json.utf8)) == .refused(WireRefused(reason: "unintelligible answer")))
        #expect(throws: DecodingError.self) {
            _ = try WireCodec.decodeResponseTolerante(Data(#"{"type":"surprise"}"#.utf8))
        }
    }
}

/// Requests the fake agent saw, in order.
nonisolated final class Registro: @unchecked Sendable {
    private let lock = NSLock()
    private var _pedidos: [WireRequest] = []
    var pedidos: [WireRequest] { lock.withLock { _pedidos } }
    func anotar(_ r: WireRequest) { lock.withLock { _pedidos.append(r) } }
    func cuantos(_ r: WireRequest) -> Int { lock.withLock { _pedidos.filter { $0 == r }.count } }
}

/// A conversation agent that answers like the core. `vistaDespues`: the changed view is pushed
/// after the answer (the core's order); otherwise before it. The app must handle both.
nonisolated func agenteConversacion(_ registro: Registro, inicial: WireConversation = vista(),
                                    captura: WireResponse = .captureStarted,
                                    texto: WireResponse = .textAccepted,
                                    vistaDespues: Bool = true) -> @Sendable (WireRequest) -> [WireResponse] {
    { req in
        registro.anotar(req)
        func con(_ v: WireConversation, _ r: WireResponse) -> [WireResponse] {
            vistaDespues ? [r, .conversation(v)] : [.conversation(v), r]
        }
        switch req {
        case .hello: return [.hello(WireHello(version: ipcVersion))]
        case .subscribeConversation: return [.conversationSubscribed, .conversation(inicial)]
        case .sendText: return con(vista(activity: .awaitingReply), texto)
        case .captureStart:
            if case .captureStarted = captura { return con(vista(activity: .recording), captura) }
            return [captura]
        case .captureEnd(let e):
            return con(vista(draft: e.then == .review ? "enciende la luz" : nil), .captureEnded)
        case .cancelTurn: return [.turnCancelSent]
        case .turnVisible: return [.turnVisibleNoted]
        case .requestMicPermission: return [.micPermission(WireMicPermission(state: .granted))]
        default: return [.refused(WireRefused(reason: "x"))]
        }
    }
}

@MainActor func esperar(_ ok: () -> Bool) async -> Bool {
    for _ in 0..<100 {
        if ok() { return true }
        try? await Task.sleep(for: .milliseconds(50))
    }
    return false
}

/// A live conversation over a real socket: the agent, its link and the model.
@MainActor func conectar(_ agent: FakeAgent) async throws -> (ConversationLink, Conversacion) {
    try await Task.sleep(for: .milliseconds(200))
    let link = ConversationLink(socketPath: agent.path, verificacion: .rutaYUsuario)
    let conv = Conversacion(link: link)
    link.start()
    #expect(await esperar { conv.vivo })
    return (link, conv)
}

func abierta(_ t: Toma) -> Bool { if case .abierta = t { true } else { false } }

@Suite(.serialized) struct ConversacionPorElSocket {
    @Test(arguments: [true, false])
    func lasRespuestasNoSeConfundenConLaVista(vistaDespues: Bool) async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro, inicial: vista([turno("t1", outcome: .done)]), vistaDespues: vistaDespues))
        let (link, _) = try await conectar(agent)
        defer { link.stop() }
        #expect(await esperar { link.vista?.turns.count == 1 })
        #expect(try await link.pedir(.sendText(WireSendText(text: "hola"))) == .textAccepted)
        #expect(await esperar { link.vista?.activity == .awaitingReply })
        #expect(try await link.pedir(.cancelTurn) == .turnCancelSent)
    }

    /// ADR-0002 mirrored: a server of the same user that is not the signed agent gets nothing.
    @Test func unServidorSinFirmarNoRecibeNada() async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro))
        try await Task.sleep(for: .milliseconds(200))
        let link = ConversationLink(socketPath: agent.path, verificacion: .agenteFirmado)
        link.start()
        defer { link.stop() }
        #expect(await esperar { link.enlace == .noDisponible })
        #expect(registro.pedidos.isEmpty)
    }

    /// The read-only status needs only the directory and user checks (an S1 agent not yet signed
    /// still shows its state); pairing needs the signed agent and sends nothing otherwise.
    @Test func elEstadoSiPeroEmparejarNo() async throws {
        let registro = Registro()
        let base = answers()
        let agent = try FakeAgent { req in registro.anotar(req); return base(req) }
        try await Task.sleep(for: .milliseconds(200))
        let estado = AgentLink(socketPath: agent.path, verificacion: .agenteFirmado)
        estado.start()
        defer { estado.stop() }
        #expect(await waitFor(estado) { $0 == .live(ready) })
        await #expect(throws: CommandError.notOurAgent) { _ = try await estado.pair(address: "green-house", code: "482913") }
        await #expect(throws: CommandError.notOurAgent) { try await estado.unpair() }
        #expect(!registro.pedidos.contains { if case .pair = $0 { true } else { false } })
        #expect(!registro.pedidos.contains(.unpair))
    }

    @Test func soloParaLaAppFirmadaSinBucle() async throws {
        let agent = try FakeAgent { req in
            if case .hello = req { return [.hello(WireHello(version: ipcVersion))] }
            return [.refused(WireRefused(reason: "needs the signed Fogar app"))]
        }
        try await Task.sleep(for: .milliseconds(200))
        let link = ConversationLink(socketPath: agent.path, verificacion: .rutaYUsuario)
        link.start()
        defer { link.stop() }
        #expect(await esperar { link.enlace == .soloAppFirmada })
        await #expect(throws: CommandError.agentNotRunning) { _ = try await link.pedir(.cancelTurn) }
        try await Task.sleep(for: .seconds(1))
        #expect(agent.accepted == 1)
    }

    @Test func unaPeticionSinRespuestaCortaLaConexionYVuelve() async throws {
        let registro = Registro()
        let base = agenteConversacion(registro)
        let agent = try FakeAgent { req in
            if case .cancelTurn = req { registro.anotar(req); return [] } // never answered
            return base(req)
        }
        let (link, _) = try await conectar(agent)
        defer { link.stop() }
        await #expect(throws: CommandError.noAnswer) { _ = try await link.pedir(.cancelTurn, plazo: .milliseconds(300)) }
        #expect(await esperar { link.enlace == .viva && agent.accepted >= 2 })
    }

    /// A request queued behind a slow one is answered in its turn, without tearing the link down.
    @Test func unaPeticionEnColaTrasOtraLenta() async throws {
        let registro = Registro()
        let agent = try FakeAgent(pausa: { if case .sendText = $0 { .seconds(1) } else { nil } }, agenteConversacion(registro))
        let (link, _) = try await conectar(agent)
        defer { link.stop() }
        async let lenta = link.pedir(.sendText(WireSendText(text: "hola")))
        try await Task.sleep(for: .milliseconds(50))
        async let detras = link.pedir(.cancelTurn)
        #expect(try await lenta == .textAccepted)
        #expect(try await detras == .turnCancelSent)
        #expect(agent.accepted == 1)
    }

    /// The permission prompt goes on its own connection: a Cancel never waits behind it, and a
    /// second request while it is open is not sent.
    @Test func permisoLentoNoBloqueaCancelar() async throws {
        let registro = Registro()
        let agent = try FakeAgent(pausa: { if case .requestMicPermission = $0 { .seconds(2) } else { nil } },
                                  agenteConversacion(registro, inicial: vista(mic: .notDetermined)))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        let permiso = Task { await conv.pedirPermiso() }
        #expect(await esperar { conv.pidiendoPermiso })
        await conv.pedirPermiso() // ignored while the prompt is open
        let t0 = ContinuousClock.now
        await conv.cancelar()
        #expect(ContinuousClock.now - t0 < .seconds(1))
        #expect(conv.aviso == nil)
        await permiso.value
        #expect(!conv.pidiendoPermiso)
        #expect(registro.cuantos(.requestMicPermission) == 1)
    }

    @Test func textoRechazadoSeQuedaEnLaCaja() async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro, texto: .textRefused(WireTextRefused(reason: .notReady))))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        conv.texto = "  enciende la luz  "
        await conv.enviar()
        #expect(conv.texto == "  enciende la luz  ")
        #expect(conv.aviso == Textos.rechazoTexto(.notReady))
        #expect(registro.pedidos.contains(.sendText(WireSendText(text: "enciende la luz"))))
    }

    @Test(arguments: [true, false])
    func clicGrabaYPararDejaElTextoEnLaCaja(vistaDespues: Bool) async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro, vistaDespues: vistaDespues))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        let t0 = Date()
        conv.pulsar(ahora: t0)
        conv.soltar(ahora: t0.addingTimeInterval(0.1)) // a click: the take stays open
        #expect(await esperar { conv.toma == .abierta(mantener: false, desde: t0, pulsado: false) && conv.grabando })
        conv.parar()
        #expect(await esperar { conv.toma == .ninguna && conv.texto == "enciende la luz" })
        #expect(registro.pedidos.contains(.captureEnd(WireCaptureEnd(then: .review))))
        #expect(conv.borradorNuevo == 1)
    }

    /// A click whose `capture_started` arrives late is still a click: nothing is sent.
    @Test func clicConCaptureStartedLento() async throws {
        let registro = Registro()
        let agent = try FakeAgent(pausa: { if case .captureStart = $0 { .milliseconds(600) } else { nil } }, agenteConversacion(registro))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        let t0 = Date()
        conv.pulsar(ahora: t0)
        conv.soltar(ahora: t0.addingTimeInterval(0.1))
        #expect(await esperar { conv.toma == .abierta(mantener: false, desde: t0, pulsado: false) })
        conv.comprobarMantener(ahora: t0.addingTimeInterval(5)) // released: never promoted
        #expect(conv.toma == .abierta(mantener: false, desde: t0, pulsado: false))
        #expect(!registro.pedidos.contains { if case .captureEnd = $0 { true } else { false } })
    }

    @Test(arguments: [true, false])
    func mantenerYSoltarEnvia(vistaDespues: Bool) async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro, vistaDespues: vistaDespues))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        let t0 = Date()
        conv.pulsar(ahora: t0)
        #expect(await esperar { abierta(conv.toma) })
        conv.comprobarMantener(ahora: t0.addingTimeInterval(0.4))
        #expect(conv.toma == .abierta(mantener: true, desde: t0, pulsado: true))
        conv.soltar(ahora: t0.addingTimeInterval(0.8))
        #expect(await esperar { registro.pedidos.contains(.captureEnd(WireCaptureEnd(then: .send))) && conv.toma == .ninguna })
        #expect(conv.texto.isEmpty)
    }

    /// Released after the threshold while the take was still opening: it sends once open.
    @Test func soltarMientrasAbreEnvia() async throws {
        let registro = Registro()
        let agent = try FakeAgent(pausa: { if case .captureStart = $0 { .milliseconds(600) } else { nil } }, agenteConversacion(registro))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        let t0 = Date()
        conv.pulsar(ahora: t0)
        conv.soltar(ahora: t0.addingTimeInterval(0.5))
        #expect(await esperar { registro.pedidos.contains(.captureEnd(WireCaptureEnd(then: .send))) && conv.toma == .ninguna })
    }

    /// Esc while opening: the take is discarded as soon as the agent opens it.
    @Test func escMientrasAbreDescarta() async throws {
        let registro = Registro()
        let agent = try FakeAgent(pausa: { if case .captureStart = $0 { .milliseconds(600) } else { nil } }, agenteConversacion(registro))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        conv.alternar()
        conv.descartar()
        #expect(await esperar { registro.pedidos.contains(.captureEnd(WireCaptureEnd(then: .discard))) && conv.toma == .ninguna })
        #expect(conv.aviso == nil)
    }

    /// The connection drops with a take open: the agent discarded it; the app forgets it quietly
    /// and comes back.
    @Test func caidaConTomaAbierta() async throws {
        let registro = Registro()
        let agent = try FakeAgent(cerrarTras: { if case .captureStart = $0 { true } else { false } }, agenteConversacion(registro))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        conv.alternar()
        #expect(await esperar { abierta(conv.toma) })
        #expect(await esperar { conv.toma == .ninguna && !conv.grabando })
        #expect(conv.aviso == nil)
        #expect(await esperar { conv.vivo && agent.accepted >= 2 })
    }

    @Test func unaTomaRechazadaLoDiceYNoQuedaAbierta() async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro, captura: .captureRefused(WireCaptureRefused(reason: .noTranscriber))))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        conv.alternar()
        #expect(await esperar { conv.aviso == Textos.rechazoToma(.noTranscriber) })
        #expect(conv.toma == .ninguna)
    }

    /// An automatic cut (silence) ends the take in the app and leaves the text in the box: the app
    /// never sends it on its own.
    @Test func trasUnCorteNuncaEnviaSola() async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        let t0 = Date()
        conv.pulsar(ahora: t0)
        #expect(await esperar { abierta(conv.toma) })
        // The agent cut the take: the next view has no `recording`, and the transcript as draft.
        link.alCambiar(vista(draft: "me quedé pensando", notice: .silenceCut))
        #expect(conv.toma == .ninguna)
        #expect(conv.texto == "me quedé pensando")
        // The release of the press that started it arrives late: it must not send anything.
        conv.soltar(ahora: t0.addingTimeInterval(2))
        try await Task.sleep(for: .milliseconds(300))
        #expect(!registro.pedidos.contains { if case .sendText = $0 { true } else { false } })
        #expect(!registro.pedidos.contains(.captureEnd(WireCaptureEnd(then: .send))))
    }

    @Test func miTurnoSeConfirmaSinVentanaYUnPushNo() async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro, inicial: vista([turno("t1"), turno("p1", .push)], esperando: "p1")))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        #expect(await esperar { registro.pedidos.contains(.turnVisible(WireTurnVisible(turn_id: "t1"))) })
        try await Task.sleep(for: .milliseconds(300))
        #expect(!registro.pedidos.contains(.turnVisible(WireTurnVisible(turn_id: "p1"))))
        conv.aLaVista = true
        #expect(await esperar { registro.pedidos.contains(.turnVisible(WireTurnVisible(turn_id: "p1"))) })
    }

    /// A busy agent gets a bounded number of tries; a refusal that will not change gets one.
    @Test(arguments: [("engine busy", Conversacion.maxIntentosVisible), ("unsupported request", 1)])
    func turnVisibleRechazadoEsAcotado(motivo: String, maximo: Int) async throws {
        let registro = Registro()
        let abierta = vista([turno("t1")])
        let agent = try FakeAgent { req in
            registro.anotar(req)
            switch req {
            case .hello: return [.hello(WireHello(version: ipcVersion))]
            case .subscribeConversation: return [.conversationSubscribed, .conversation(abierta)]
            case .turnVisible: return [.refused(WireRefused(reason: motivo))]
            case .cancelTurn: return [.turnCancelSent, .conversation(abierta)]
            default: return [.refused(WireRefused(reason: "x"))]
            }
        }
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        let visible = WireRequest.turnVisible(WireTurnVisible(turn_id: "t1"))
        #expect(await esperar { registro.cuantos(visible) >= 1 })
        // New views keep arriving meanwhile; the backoff is 0.5 s, then 1 s.
        for _ in 0..<6 {
            try await Task.sleep(for: .milliseconds(500))
            await conv.cancelar()
        }
        #expect(registro.cuantos(visible) == maximo)
    }

    @Test func traConfirmar() {
        #expect(Conversacion.tras(.turnVisibleNoted, intentos: 0) == .anotado)
        #expect(Conversacion.tras(nil, intentos: 0) == .siguienteVista)
        #expect(Conversacion.tras(.refused(WireRefused(reason: "engine busy")), intentos: 0) == .reintentar(.milliseconds(500)))
        #expect(Conversacion.tras(.refused(WireRefused(reason: "engine busy")), intentos: 1) == .reintentar(.seconds(1)))
        #expect(Conversacion.tras(.refused(WireRefused(reason: "engine busy")), intentos: 2) == .descartar)
        #expect(Conversacion.tras(.refused(WireRefused(reason: "needs the signed Fogar app")), intentos: 0) == .descartar)
    }

    /// A confirmation the agent did not note is retried on the next view, and only once noted.
    @Test func turnVisibleSeReintenta() async throws {
        let registro = Registro()
        let abierta = vista([turno("t1")])
        let agent = try FakeAgent { req in
            registro.anotar(req)
            switch req {
            case .hello: return [.hello(WireHello(version: ipcVersion))]
            case .subscribeConversation: return [.conversationSubscribed, .conversation(abierta)]
            case .turnVisible:
                return registro.cuantos(req) == 1 ? [.refused(WireRefused(reason: "engine busy"))] : [.turnVisibleNoted]
            case .cancelTurn: return [.turnCancelSent, .conversation(abierta)]
            default: return [.refused(WireRefused(reason: "x"))]
            }
        }
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        let visible = WireRequest.turnVisible(WireTurnVisible(turn_id: "t1"))
        #expect(await esperar { registro.cuantos(visible) == 1 })
        try await Task.sleep(for: .milliseconds(200))
        await conv.cancelar() // any request that brings a new view
        #expect(await esperar { registro.cuantos(visible) == 2 })
        await conv.cancelar()
        try await Task.sleep(for: .milliseconds(300))
        #expect(registro.cuantos(visible) == 2)
    }
}
