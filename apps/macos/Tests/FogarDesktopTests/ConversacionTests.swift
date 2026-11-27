import AppKit
import FogarWire
import Foundation
import Testing
@testable import FogarDesktop

nonisolated func turno(_ id: String, _ source: WireSource = .mine, outcome: WireOutcome? = nil, reply: String = "") -> WireTurn {
    WireTurn(turn_id: id, source: source, by_name: nil, by_self: source == .mine, reply: reply,
             progress: outcome == nil ? .thinking : nil, tool: nil, outcome: outcome, error_code: nil)
}

nonisolated func mensaje(_ texto: String, _ state: WireMessageState = .delivered, mio: Bool = true,
                         de: String? = nil, codigo: String? = nil) -> WireMessage {
    WireMessage(id: "m-\(texto)", text: texto, mine: mio, from_name: de, state: state, error_code: codigo)
}

/// A view: `entradas` in order (messages and turns), or just `turns`.
nonisolated func vista(_ turns: [WireTurn] = [], entradas: [WireEntry] = [], id: String? = "c1",
                       activity: WireActivity = .idle, live: String = "", draft: String? = nil,
                       mic: WireMic = .granted, transcriber: WireService = .up, notice: WireNotice? = nil,
                       soloLectura: Bool = false) -> WireConversation {
    WireConversation(id: id, activity: activity, live: live, draft: draft, entries: entradas + turns.map { .turn($0) },
                     mic: mic, transcriber: transcriber, notice: notice, read_only: soloLectura)
}

nonisolated func resumen(_ id: String, _ titulo: String?, enMarcha: Bool = false) -> WireSummary {
    WireSummary(id: id, title: titulo, private: true, last_turn_at: nil, running: enMarcha)
}

@Suite struct Cancelar {
    @Test func cancelarTieneSentido() {
        #expect(!Conversacion.cancelable(vista([turno("t1", outcome: .done)])))
        #expect(Conversacion.cancelable(vista([turno("t1")])))
        #expect(Conversacion.cancelable(vista(activity: .awaitingReply)))
        // A message waiting behind a turn (that turn may run on another device).
        #expect(Conversacion.cancelable(vista(entradas: [.message(mensaje("otra", .pending))])))
        // A turn from history has no id and is never «running».
        #expect(!Conversacion.cancelable(vista([turno("", outcome: nil)])))
    }
}

/// What a message says under its bubble.
@Suite struct Mensajes {
    @Test func notas() {
        #expect(Textos.notaMensaje(mensaje("hola")) == nil)
        #expect(Textos.notaMensaje(mensaje("hola", .past, mio: false)) == nil)
        #expect(Textos.notaMensaje(mensaje("hola", mio: false, de: "Cocina")) == "Enviado desde Cocina")
        #expect(Textos.notaMensaje(mensaje("hola", mio: false, de: "   ")) == nil)
        #expect(Textos.notaMensaje(mensaje("hola", .pending)) == "Pendiente: va cuando Fogar termine.")
        #expect(Textos.notaMensaje(mensaje("hola", .failed, codigo: "missing_scope"))?.contains("no puede escribir") == true)
        #expect(Textos.notaMensaje(mensaje("hola", .failed, codigo: "not_sent"))?.hasPrefix("No se envió") == true)
        #expect(Textos.notaMensaje(mensaje("hola", .failed, codigo: "unknown"))
            == "Puede que se enviara: Fogar tardó en contestar. Mira si aparece antes de volver a escribirlo.")
        #expect(Textos.tituloConversacion(resumen("c", "  La serie ")) == "La serie")
        #expect(Textos.tituloConversacion(resumen("c", nil)) == "Conversación sin título")
        #expect(Textos.tituloConversacion(resumen("c", " ")) == "Conversación sin título")
        #expect(Textos.autorMensaje(mensaje("x")) == "Tú")
        #expect(Textos.autorMensaje(mensaje("x", mio: false, de: String(repeating: "a", count: 60))).count == 40)
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
            // Dictation is off until PR 5: no transcriber reported `up`, no take offered.
            (.viva, vista(transcriber: .unknown), true, .sinTranscriptor),
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
        let raro = WireTurn(turn_id: "t", source: .mine, by_name: nil, by_self: true, reply: "", progress: nil, tool: nil,
                            outcome: .error, error_code: "x\ny")
        #expect(Textos.desenlace(raro) == "Algo falló en el gateway.")
    }

    @Test func erroresDelOrquestadorConSuFrase() {
        func turno(_ code: String) -> WireTurn {
            WireTurn(turn_id: "t", source: .mine, by_name: nil, by_self: true, reply: "", progress: nil, tool: nil,
                     outcome: .error, error_code: code)
        }
        let codigos = ["orchestrator_auth_failed", "orchestrator_model_unavailable", "orchestrator_rate_limited",
                       "orchestrator_timeout", "orchestrator_agent_unavailable"]
        #expect(Set(codigos.compactMap { Textos.desenlace(turno($0)) }).count == codigos.count)
        #expect(Textos.desenlace(turno("orchestrator_timeout")) == "Fogar tardó demasiado en responder.")
        #expect(Textos.desenlace(turno("orchestrator_error")) == "Algo falló en el gateway (orchestrator_error).")
    }

    @Test func cadaRechazoDelAgenteConSuFrase() {
        let motivos = ["engine busy", "needs the signed Fogar app", "unsupported request", "ipc version mismatch", "otra cosa"]
        #expect(Set(motivos.map(Textos.rechazo)).count == motivos.count)
    }
}

/// A newer agent must not cost the connection: unknown values read as their closest meaning.
@Suite struct LecturaTolerante {
    @Test func valoresDesconocidosEnLaVista() throws {
        let json = #"{"type":"conversation","data":{"id":"c1","activity":"dancing","live":"","draft":null,"entries":[{"type":"turn","data":{"turn_id":"t1","source":"robot","by_name":null,"by_self":false,"reply":"hola","progress":"juggling","tool":null,"outcome":null,"error_code":null}},{"type":"photo","data":{"url":"x"}},{"type":"message","data":{"id":"m","text":"a","mine":true,"from_name":null,"state":"queued","error_code":null}},{"type":"turn","data":{"turn_id":"t2","source":"mine","by_name":"Cocina","by_self":false,"reply":"b","progress":null,"tool":null,"outcome":"exploded","error_code":null}}],"mic":"maybe","transcriber":"sleepy","notice":"new_notice","read_only":false}}"#
        guard case .conversation(let v) = try WireCodec.decodeResponseTolerante(Data(json.utf8)) else {
            Issue.record("not a conversation")
            return
        }
        #expect(v.activity == .awaitingReply && v.mic == .unknown && v.transcriber == .unknown && v.notice == nil)
        #expect(v.entries.count == 3, "an unknown kind of entry is left out")
        guard case .turn(let t1) = v.entries[0], case .message(let m) = v.entries[1], case .turn(let t2) = v.entries[2] else {
            Issue.record("entries out of order")
            return
        }
        #expect(t1.source == .push && t1.progress == .working)
        #expect(m.state == .delivered)
        #expect(t2.outcome == .error && t2.by_name == "Cocina")
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
                                    lista: [WireSummary] = [resumen("c1", "La serie")],
                                    captura: WireResponse = .captureStarted,
                                    texto: WireResponse = .textAccepted,
                                    cancelar: WireResponse = .turnCancelSent,
                                    vistaDespues: Bool = true) -> @Sendable (WireRequest) -> [WireResponse] {
    { req in
        registro.anotar(req)
        func con(_ v: WireConversation, _ r: WireResponse) -> [WireResponse] {
            vistaDespues ? [r, .conversation(v)] : [.conversation(v), r]
        }
        switch req {
        case .hello: return [.hello(WireHello(version: ipcVersion))]
        case .subscribeConversation: return [.conversationSubscribed, .conversation(inicial)]
        case .listConversations: return [.listSubscribed, .conversations(WireConversations(conversations: lista))]
        case .openConversation(let o): return con(vista(id: o.id), .conversationOpened)
        case .newConversation: return con(vista(id: nil), .conversationOpened)
        case .sendText: return con(vista(activity: .awaitingReply), texto)
        case .captureStart:
            if case .captureStarted = captura { return con(vista(activity: .recording), captura) }
            return [captura]
        case .captureEnd(let e):
            return con(vista(draft: e.then == .review ? "enciende la luz" : nil), .captureEnded)
        case .cancelTurn: return [cancelar]
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

@Suite(.serialized) struct ConversacionPorElSocket {
    @Test(arguments: [true, false])
    func lasRespuestasNoSeConfundenConLaVista(vistaDespues: Bool) async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro, inicial: vista([turno("t1", outcome: .done)]), vistaDespues: vistaDespues))
        let (link, _) = try await conectar(agent)
        defer { link.stop() }
        #expect(await esperar { link.vista?.entries.count == 1 })
        #expect(link.lista == [resumen("c1", "La serie")])
        #expect(try await link.pedir(.sendText(WireSendText(text: "hola"))) == .textAccepted)
        #expect(await esperar { link.vista?.activity == .awaitingReply })
        #expect(try await link.pedir(.cancelTurn) == .turnCancelSent)
        #expect(try await link.pedir(.newConversation) == .conversationOpened)
        #expect(await esperar { link.vista?.id == nil })
    }

    /// The list is pushed unasked, like the view: a push between a request and its answer never
    /// takes the answer's place.
    @Test func laListaLlegaSinPedirla() async throws {
        let registro = Registro()
        let base = agenteConversacion(registro)
        let agent = try FakeAgent { req in
            if case .openConversation = req {
                registro.anotar(req)
                return [.conversations(WireConversations(conversations: [resumen("c2", "Nueva", enMarcha: true)])),
                        .conversationOpened, .conversation(vista(id: "c2"))]
            }
            return base(req)
        }
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        await conv.abrir("c2")
        #expect(conv.aviso == nil)
        #expect(await esperar { link.vista?.id == "c2" && link.lista.first?.running == true })
        // Opening what is already open sends nothing.
        await conv.abrir("c2")
        #expect(registro.cuantos(.openConversation(WireOpen(id: "c2"))) == 1)
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
    /// still shows its state); the account needs the signed agent and sends nothing otherwise
    /// (the password never reaches a socket that is not provably our agent).
    @Test func elEstadoSiPeroLaCuentaNo() async throws {
        let registro = Registro()
        let base = answers()
        let agent = try FakeAgent { req in registro.anotar(req); return base(req) }
        try await Task.sleep(for: .milliseconds(200))
        let estado = AgentLink(socketPath: agent.path, verificacion: .agenteFirmado)
        estado.start()
        defer { estado.stop() }
        #expect(await waitFor(estado) { $0 == .live(ready) })
        await #expect(throws: CommandError.notOurAgent) {
            _ = try await estado.login(address: "https://casa.ts.net", username: "sito", password: "fogar")
        }
        await #expect(throws: CommandError.notOurAgent) { _ = try await estado.logout() }
        #expect(!registro.pedidos.contains { if case .login = $0 { true } else { false } })
        #expect(!registro.pedidos.contains { if case .logout = $0 { true } else { false } })
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

    /// Stop while the agent has no link: an honest line, not silence.
    @Test func pararSinConexionLoDice() async throws {
        let agent = try FakeAgent(agenteConversacion(Registro(), cancelar: .turnCancelNotReady))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        await conv.cancelar()
        #expect(conv.aviso == "Fogar no está conectado: no se pudo parar.")
    }

    /// Click the microphone, then stop: the transcript waits in the box (`review`), never sent.
    @Test(arguments: [true, false])
    func clicGrabaYPararDejaElTextoEnLaCaja(vistaDespues: Bool) async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro, vistaDespues: vistaDespues))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        conv.grabar()
        #expect(await esperar { conv.toma == .abierta && conv.grabando })
        #expect(registro.pedidos.contains(.captureStart(WireCaptureStart(gesture: .toggle))))
        conv.parar()
        #expect(await esperar { conv.toma == .ninguna && conv.texto == "enciende la luz" })
        #expect(registro.pedidos.contains(.captureEnd(WireCaptureEnd(then: .review))))
        #expect(!registro.pedidos.contains { if case .sendText = $0 { true } else { false } })
        #expect(conv.borradorNuevo == 1)
    }

    /// Click the microphone, then send: the take ends with `send` and nothing waits in the box.
    @Test(arguments: [true, false])
    func clicGrabaYEnviar(vistaDespues: Bool) async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro, vistaDespues: vistaDespues))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        conv.grabar()
        #expect(await esperar { conv.toma == .abierta })
        conv.grabar() // a second click while recording opens nothing
        conv.enviarToma()
        #expect(await esperar { registro.pedidos.contains(.captureEnd(WireCaptureEnd(then: .send))) && conv.toma == .ninguna })
        #expect(registro.cuantos(.captureStart(WireCaptureStart(gesture: .toggle))) == 1)
        #expect(conv.texto.isEmpty)
    }

    /// Stop or send pressed while the take is still opening: it ends that way as soon as it opens.
    @Test(arguments: [WireThen.review, .send])
    func terminarMientrasAbre(then: WireThen) async throws {
        let registro = Registro()
        let agent = try FakeAgent(pausa: { if case .captureStart = $0 { .milliseconds(600) } else { nil } }, agenteConversacion(registro))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        conv.grabar()
        #expect(conv.toma == .abriendo(fin: nil))
        if then == .send { conv.enviarToma() } else { conv.parar() }
        #expect(conv.toma == .abriendo(fin: then))
        #expect(await esperar { registro.pedidos.contains(.captureEnd(WireCaptureEnd(then: then))) && conv.toma == .ninguna })
        #expect(registro.cuantos(.captureEnd(WireCaptureEnd(then: then))) == 1)
    }

    /// Esc while opening, with the conversation on screen: discarded as soon as the agent opens it.
    /// Without a take, Esc and Return are left to the text field.
    @Test func escMientrasAbreDescarta() async throws {
        let registro = Registro()
        let agent = try FakeAgent(pausa: { if case .captureStart = $0 { .milliseconds(600) } else { nil } }, agenteConversacion(registro))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        conv.aLaVista = true
        #expect(!conv.tecla(.escape))
        #expect(!conv.tecla(.intro))
        conv.grabar()
        #expect(conv.tecla(.escape))
        #expect(await esperar { registro.pedidos.contains(.captureEnd(WireCaptureEnd(then: .discard))) && conv.toma == .ninguna })
        #expect(conv.aviso == nil)
    }

    /// M2: while opening, the first way out wins: Esc then Return discards, never sends.
    @Test func escYLuegoIntroMientrasAbreDescarta() async throws {
        let registro = Registro()
        let agent = try FakeAgent(pausa: { if case .captureStart = $0 { .milliseconds(600) } else { nil } }, agenteConversacion(registro))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        conv.aLaVista = true
        conv.grabar()
        #expect(conv.tecla(.escape))
        _ = conv.tecla(.intro)
        conv.enviarToma()
        #expect(conv.toma == .abriendo(fin: .discard))
        #expect(await esperar { registro.pedidos.contains(.captureEnd(WireCaptureEnd(then: .discard))) && conv.toma == .ninguna })
        #expect(!registro.pedidos.contains(.captureEnd(WireCaptureEnd(then: .send))))
    }

    /// T2: a slow opening with nothing pressed stays open: no `capture_end` of its own.
    @Test func aperturaLentaSinGestoNoTermina() async throws {
        let registro = Registro()
        let agent = try FakeAgent(pausa: { if case .captureStart = $0 { .milliseconds(600) } else { nil } }, agenteConversacion(registro))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        conv.grabar()
        #expect(await esperar { conv.toma == .abierta && conv.grabando })
        try await Task.sleep(for: .milliseconds(300))
        #expect(conv.toma == .abierta)
        #expect(!registro.pedidos.contains { if case .captureEnd = $0 { true } else { false } })
    }

    /// T1: with the conversation out of sight, or the key event from another window, Esc and
    /// Return are not the take's: they return false and leave it open.
    @Test func escEIntroFueraDeLaConversacionNoTocanLaToma() async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        conv.grabar()
        #expect(await esperar { conv.toma == .abierta })
        conv.aLaVista = false
        #expect(!conv.tecla(.escape))
        #expect(!conv.tecla(.intro))
        conv.aLaVista = true
        #expect(!conv.tecla(.escape, enLaConversacion: false))
        #expect(!conv.tecla(.intro, enLaConversacion: false))
        // No conversation window known: nothing counts as coming from it.
        #expect(!conv.admiteTeclasDeToma(ventanaDelEvento: 0))
        try await Task.sleep(for: .milliseconds(200))
        #expect(conv.toma == .abierta)
        #expect(!registro.pedidos.contains { if case .captureEnd = $0 { true } else { false } })
    }

    /// B3: while the take is closing, Esc and Return are not swallowed.
    @Test func escEIntroNoSeTraganAlCerrar() async throws {
        let registro = Registro()
        let agent = try FakeAgent(pausa: { if case .captureEnd = $0 { .milliseconds(600) } else { nil } }, agenteConversacion(registro))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        conv.aLaVista = true
        conv.grabar()
        #expect(await esperar { conv.toma == .abierta })
        conv.parar()
        #expect(conv.toma == .cerrando)
        #expect(!conv.tecla(.escape))
        #expect(!conv.tecla(.intro))
        #expect(await esperar { conv.toma == .ninguna })
        #expect(registro.cuantos(.captureEnd(WireCaptureEnd(then: .review))) == 1)
        #expect(!registro.pedidos.contains(.captureEnd(WireCaptureEnd(then: .discard))))
    }

    /// B4: installing the keyboard monitor twice keeps the first one.
    @Test func escucharTecladoEsIdempotente() async throws {
        _ = NSApplication.shared
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        #expect(conv.monitor == nil)
        conv.escucharTeclado()
        let primero = try #require(conv.monitor) as AnyObject
        conv.escucharTeclado()
        #expect(try #require(conv.monitor) as AnyObject === primero)
    }

    /// Return in an open take sends it, like the send button.
    @Test func introEnLaTomaEnvia() async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        conv.aLaVista = true
        conv.grabar()
        #expect(await esperar { conv.toma == .abierta })
        #expect(conv.tecla(.intro))
        #expect(await esperar { registro.pedidos.contains(.captureEnd(WireCaptureEnd(then: .send))) && conv.toma == .ninguna })
    }

    /// The connection drops with a take open: the agent discarded it; the app forgets it quietly
    /// and comes back.
    @Test func caidaConTomaAbierta() async throws {
        let registro = Registro()
        let agent = try FakeAgent(cerrarTras: { if case .captureStart = $0 { true } else { false } }, agenteConversacion(registro))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        conv.grabar()
        #expect(await esperar { conv.toma == .abierta })
        #expect(await esperar { conv.toma == .ninguna && !conv.grabando })
        #expect(conv.aviso == nil)
        #expect(await esperar { conv.vivo && agent.accepted >= 2 })
    }

    @Test func unaTomaRechazadaLoDiceYNoQuedaAbierta() async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro, captura: .captureRefused(WireCaptureRefused(reason: .noTranscriber))))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        conv.grabar()
        #expect(await esperar { conv.aviso == Textos.rechazoToma(.noTranscriber) })
        #expect(conv.toma == .ninguna)
    }

    /// An automatic cut (silence, safety) ends the take in the app and leaves the text in the box:
    /// the app never sends it on its own, and a Return pressed late does not either.
    @Test(arguments: [WireNotice.silenceCut, .safetyCut])
    func trasUnCorteNuncaEnviaSola(corte: WireNotice) async throws {
        let registro = Registro()
        let base = agenteConversacion(registro)
        // The agent opens the take and cuts it at once: a view without `recording`, the
        // transcript as draft, over the real socket (the link's view and the model's agree).
        let agent = try FakeAgent { req in
            guard case .captureStart = req else { return base(req) }
            registro.anotar(req)
            return [.captureStarted, .conversation(vista(activity: .recording, live: "me quedé pensando")),
                    .conversation(vista(draft: "me quedé pensando", notice: corte))]
        }
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        conv.aLaVista = true
        conv.grabar()
        #expect(await esperar { conv.texto == "me quedé pensando" && conv.toma == .ninguna && !conv.grabando })
        #expect(!conv.tecla(.intro))
        try await Task.sleep(for: .milliseconds(300))
        #expect(!registro.pedidos.contains { if case .sendText = $0 { true } else { false } })
        #expect(!registro.pedidos.contains { if case .captureEnd = $0 { true } else { false } })
    }

    /// #33: ⌘. cancels while Fogar answers, whatever has focus and even with the conversation out
    /// of sight (menu bar panel, another section): the app itself sends `cancel_turn`. Before, ⌘.
    /// lived only on an in-window button and nothing in the app reached the agent.
    @Test func comandoPuntoCancelaMientrasResponde() async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro, inicial: vista([turno("t1")])))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        #expect(await esperar { conv.cancelable })
        #expect(!conv.aLaVista)
        #expect(conv.tecla(.cancelar))
        #expect(await esperar { registro.cuantos(.cancelTurn) == 1 })
        #expect(conv.aviso == nil)
    }

    /// Nothing to cancel: ⌘. is left alone (to the field or the system) and nothing is sent.
    @Test func comandoPuntoSinNadaQueCancelar() async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro, inicial: vista([turno("t1", outcome: .done)])))
        let (link, conv) = try await conectar(agent)
        defer { link.stop() }
        #expect(await esperar { link.vista?.entries.count == 1 })
        #expect(!conv.tecla(.cancelar))
        try await Task.sleep(for: .milliseconds(200))
        #expect(registro.cuantos(.cancelTurn) == 0)
    }
}

/// #33: which key events the app keeps for itself.
@Suite struct Teclas {
    func evento(_ c: String, _ mods: NSEvent.ModifierFlags, keyCode: UInt16, repetido: Bool = false) -> NSEvent? {
        NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: mods, timestamp: 0, windowNumber: 0,
                         context: nil, characters: c, charactersIgnoringModifiers: c, isARepeat: repetido, keyCode: keyCode)
    }

    @Test func comandoPuntoEsCancelar() throws {
        #expect(Tecla.de(try #require(evento(".", .command, keyCode: 47))) == .cancelar)
        // Caps Lock or the function key do not change it.
        #expect(Tecla.de(try #require(evento(".", [.command, .capsLock], keyCode: 47))) == .cancelar)
    }

    @Test func soloEsasTeclas() throws {
        #expect(Tecla.de(try #require(evento("\u{1b}", [], keyCode: 53))) == .escape)
        #expect(Tecla.de(try #require(evento("\r", [], keyCode: 36))) == .intro)
        #expect(Tecla.de(try #require(evento("\u{3}", .numericPad, keyCode: 76))) == .intro) // keypad Enter
        #expect(Tecla.de(try #require(evento(".", [], keyCode: 47))) == nil)
        #expect(Tecla.de(try #require(evento(".", [.command, .shift], keyCode: 47))) == nil)
        #expect(Tecla.de(try #require(evento("\r", .option, keyCode: 36))) == nil) // Option-Return: a new line
        #expect(Tecla.de(try #require(evento(".", .command, keyCode: 47, repetido: true))) == nil)
    }
}
