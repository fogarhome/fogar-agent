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
    @Test func cadaEstadoConSuFrase() {
        let casos: [(EnlaceConversacion, WireConversation?, Bool, EstadoMicro)] = [
            (.buscando, nil, true, .sinConexion),
            (.sinAgente, nil, true, .sinConexion),
            (.soloAppFirmada, nil, true, .soloAppFirmada),
            (.viva, vista(), false, .sinConexion),
            (.viva, vista(mic: .notDetermined), true, .pedirPermiso),
            (.viva, vista(mic: .denied), true, .denegado),
            (.viva, vista(mic: .restricted), true, .denegado),
            (.viva, vista(transcriber: .down), true, .sinTranscriptor),
            (.viva, vista(transcriber: .unknown), true, .listo),
            (.viva, vista(), true, .listo),
        ]
        for (enlace, v, listo, esperado) in casos {
            #expect(Conversacion.estadoMicro(enlace: enlace, vista: v, listo: listo) == esperado)
        }
        let frases = Set([EstadoMicro.listo, .sinConexion, .soloAppFirmada, .sinTranscriptor, .pedirPermiso, .denegado].map(Textos.micro))
        #expect(frases.count == 6)
    }

    @Test func elCorteAutomaticoDiceQueNoSeEnvio() {
        #expect(Textos.aviso(.silenceCut) == "He parado de grabar porque no te oía. Revisa el texto y envíalo.")
        #expect(Textos.progreso(.tool, herramienta: "ha_call_service") == "Usando ha_call_service…")
        #expect(Textos.progreso(.thinking, herramienta: nil) == "Pensando…")
    }
}

/// Requests the fake agent saw, in order.
nonisolated final class Registro: @unchecked Sendable {
    private let lock = NSLock()
    private var _pedidos: [WireRequest] = []
    var pedidos: [WireRequest] { lock.withLock { _pedidos } }
    func anotar(_ r: WireRequest) { lock.withLock { _pedidos.append(r) } }
}

/// A conversation agent: answers like the core, with a view pushed BEFORE each answer (the order
/// the app must not mistake for an answer).
nonisolated func agenteConversacion(_ registro: Registro, inicial: WireConversation = vista(),
                                    captura: WireResponse = .captureStarted,
                                    texto: WireResponse = .textAccepted) -> @Sendable (WireRequest) -> [WireResponse] {
    { req in
        registro.anotar(req)
        switch req {
        case .hello: return [.hello(WireHello(version: ipcVersion))]
        case .subscribeConversation: return [.conversationSubscribed, .conversation(inicial)]
        case .sendText: return [.conversation(vista(activity: .awaitingReply)), texto]
        case .captureStart: return [.conversation(vista(activity: .recording, live: "")), captura]
        case .captureEnd(let e):
            return [.conversation(vista(draft: e.then == .review ? "enciende la luz" : nil)), .captureEnded]
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

@Suite(.serialized) struct ConversacionPorElSocket {
    @Test func seSuscribeYLasRespuestasNoSeConfundenConLaVista() async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro, inicial: vista([turno("t1", outcome: .done)])))
        try await Task.sleep(for: .milliseconds(200))
        let link = ConversationLink(socketPath: agent.path)
        link.start()
        defer { link.stop() }
        #expect(await esperar { link.enlace == .viva && link.vista?.turns.count == 1 })
        // A pushed view arrives before the answer: the answer is still the answer.
        #expect(try await link.pedir(.sendText(WireSendText(text: "hola"))) == .textAccepted)
        #expect(link.vista?.activity == .awaitingReply)
        #expect(try await link.pedir(.cancelTurn) == .turnCancelSent)
    }

    @Test func soloParaLaAppFirmada() async throws {
        let agent = try FakeAgent { req in
            if case .hello = req { return [.hello(WireHello(version: ipcVersion))] }
            return [soloAppFirmadaRespuesta]
        }
        try await Task.sleep(for: .milliseconds(200))
        let link = ConversationLink(socketPath: agent.path)
        link.start()
        defer { link.stop() }
        #expect(await esperar { link.enlace == .soloAppFirmada })
        await #expect(throws: CommandError.agentNotRunning) { _ = try await link.pedir(.cancelTurn) }
        // A permanent answer: no reconnect loop.
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
        try await Task.sleep(for: .milliseconds(200))
        let link = ConversationLink(socketPath: agent.path)
        link.start()
        defer { link.stop() }
        #expect(await esperar { link.enlace == .viva })
        await #expect(throws: CommandError.noAnswer) { _ = try await link.pedir(.cancelTurn, plazo: .milliseconds(300)) }
        #expect(await esperar { link.enlace == .viva && agent.accepted >= 2 })
    }

    @Test func textoEnviadoSeVaDeLaCajaYUnoRechazadoSeQueda() async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro, texto: .textRefused(WireTextRefused(reason: .notReady))))
        try await Task.sleep(for: .milliseconds(200))
        let link = ConversationLink(socketPath: agent.path)
        let conv = Conversacion(link: link)
        link.start()
        defer { link.stop() }
        #expect(await esperar { conv.vivo })
        conv.texto = "  enciende la luz  "
        await conv.enviar()
        #expect(conv.texto == "  enciende la luz  ")
        #expect(conv.aviso == Textos.rechazoTexto(.notReady))
        #expect(registro.pedidos.contains(.sendText(WireSendText(text: "enciende la luz"))))
    }

    @Test func clicGrabaYPararDejaElTextoEnLaCaja() async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro))
        try await Task.sleep(for: .milliseconds(200))
        let link = ConversationLink(socketPath: agent.path)
        let conv = Conversacion(link: link)
        link.start()
        defer { link.stop() }
        #expect(await esperar { conv.vivo })
        let t0 = Date()
        conv.pulsar(ahora: t0)
        conv.soltar(ahora: t0.addingTimeInterval(0.1)) // a click: the take stays open
        #expect(await esperar { if case .abierta(false, _, false) = conv.toma { true } else { false } })
        #expect(conv.grabando)
        conv.parar()
        #expect(await esperar { conv.toma == .ninguna && conv.texto == "enciende la luz" })
        #expect(registro.pedidos.contains(.captureEnd(WireCaptureEnd(then: .review))))
        // The same draft pushed again is not copied twice.
        #expect(conv.borradorNuevo == 1)
    }

    @Test func mantenerYSoltarEnvia() async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro))
        try await Task.sleep(for: .milliseconds(200))
        let link = ConversationLink(socketPath: agent.path)
        let conv = Conversacion(link: link)
        link.start()
        defer { link.stop() }
        #expect(await esperar { conv.vivo })
        let t0 = Date()
        conv.pulsar(ahora: t0)
        #expect(await esperar { if case .abierta = conv.toma { true } else { false } })
        conv.soltar(ahora: t0.addingTimeInterval(0.8))
        #expect(await esperar { registro.pedidos.contains(.captureEnd(WireCaptureEnd(then: .send))) })
        #expect(await esperar { conv.toma == .ninguna })
        #expect(conv.texto.isEmpty)
    }

    @Test func unaTomaRechazadaLoDiceYNoQuedaAbierta() async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro, captura: .captureRefused(WireCaptureRefused(reason: .noTranscriber))))
        try await Task.sleep(for: .milliseconds(200))
        let link = ConversationLink(socketPath: agent.path)
        let conv = Conversacion(link: link)
        link.start()
        defer { link.stop() }
        #expect(await esperar { conv.vivo })
        conv.alternar()
        #expect(await esperar { conv.aviso == Textos.rechazoToma(.noTranscriber) })
        #expect(conv.toma == .ninguna)
    }

    @Test func miTurnoSeConfirmaSinVentanaYUnPushNo() async throws {
        let registro = Registro()
        let agent = try FakeAgent(agenteConversacion(registro, inicial: vista([turno("t1"), turno("p1", .push)], esperando: "p1")))
        try await Task.sleep(for: .milliseconds(200))
        let link = ConversationLink(socketPath: agent.path)
        let conv = Conversacion(link: link)
        link.start()
        defer { link.stop() }
        #expect(await esperar { registro.pedidos.contains(.turnVisible(WireTurnVisible(turn_id: "t1"))) })
        try await Task.sleep(for: .milliseconds(300))
        #expect(!registro.pedidos.contains(.turnVisible(WireTurnVisible(turn_id: "p1"))))
        // The conversation comes into view: now the push may sound.
        conv.aLaVista = true
        #expect(await esperar { registro.pedidos.contains(.turnVisible(WireTurnVisible(turn_id: "p1"))) })
    }
}

nonisolated let soloAppFirmadaRespuesta = WireResponse.refused(WireRefused(reason: "needs the signed Fogar app"))
