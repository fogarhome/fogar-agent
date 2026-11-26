import FogarWire
import Foundation
import Network
import Testing
@testable import FogarDesktop

/// A scripted agent on a real Unix socket: answers each request with `respond`.
nonisolated final class FakeAgent: @unchecked Sendable {
    /// Connections accepted so far (to catch reconnect loops).
    private let lock = NSLock()
    private var _accepted = 0
    var accepted: Int { lock.withLock { _accepted } }
    /// A private directory (0700, ours), like the agent's `run/`: the app refuses any other.
    let dir = NSTemporaryDirectory() + "fogar-t-\(UUID().uuidString.prefix(8))"
    var path: String { dir + "/a.sock" }
    private var listener: NWListener?

    /// `dropAfterSubscribe`: close each subscribed connection this long after its first event
    /// (an agent restart, as the app sees it). `pausa`: wait this long before answering a request
    /// (the connection's later requests wait behind it, like the core's).
    init(dropAfterSubscribe: Duration? = nil, pausa: (@Sendable (WireRequest) -> Duration?)? = nil,
         cerrarTras: (@Sendable (WireRequest) -> Bool)? = nil,
         _ respond: @escaping @Sendable (WireRequest) -> [WireResponse]) throws {
        try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: false,
                                                attributes: [.posixPermissions: 0o700])
        unlink(path)
        let params = NWParameters.tcp
        params.requiredLocalEndpoint = .unix(path: path)
        let l = try NWListener(using: params)
        l.newConnectionHandler = { [weak self] conn in
            self?.lock.withLock { self?._accepted += 1 }
            Task {
                let c = FramedConnection(accepted: conn)
                do {
                    try await c.start()
                    while true {
                        let req = try WireCodec.decodeRequest(try await c.receive())
                        if let d = pausa?(req) { try await Task.sleep(for: d) }
                        for r in respond(req) { try await c.send(WireCodec.encode(r)) }
                        if cerrarTras?(req) == true {
                            try await Task.sleep(for: .milliseconds(400))
                            c.cancel()
                            return
                        }
                        if case .subscribe = req, let raw = Self.rawAfterSubscribe { try await c.send(Data(raw.utf8)) }
                        if case .subscribe = req, let d = dropAfterSubscribe {
                            try await Task.sleep(for: d)
                            c.cancel()
                            return
                        }
                    }
                } catch {}
            }
        }
        l.start(queue: .global())
        listener = l
    }

    /// A raw frame body pushed after `subscribed` (to feed garbage).
    nonisolated(unsafe) static var rawAfterSubscribe: String?

    deinit {
        listener?.cancel()
        unlink(path)
        try? FileManager.default.removeItem(atPath: dir)
    }
}

nonisolated let ready = WireStatus(link: "ready", phase: .ready,
                       gateway: WireGateway(address: "http://green-house:8004", name: "green-house", version: "1.29.0", client_name: "Mac", user: ""),
                       sessions: 1, turns_completed: 2, boot_mode: "normal", peer_check: .sameSigner, transcriber: .unknown, tts: .unknown)

func answers(version: UInt32 = ipcVersion, login: WireResponse = .loggedIn(ready.gateway!),
             logout: WireResponse = .loggedOut(WireLoggedOut(gateway_reached: true))) -> @Sendable (WireRequest) -> [WireResponse] {
    { req in
        switch req {
        case .hello: [.hello(WireHello(version: version))]
        case .subscribe: [.subscribed, .event(ready)]
        case .discover: [.event(ready), .discovered(WireDiscovered(gateways: []))] // a stray event must not be the answer
        case .login: [.event(ready), login]
        case .logout: [logout]
        case .reconnect: [.event(ready), .reconnecting]
        case .status: [.status(ready)]
        case .subscribeConversation, .sendText, .captureStart, .captureEnd, .cancelTurn,
             .turnVisible, .requestMicPermission:
            [.refused(WireRefused(reason: "not in these tests"))]
        }
    }
}

/// Polls `link.state` until `ok` or 5 s pass.
func waitFor(_ link: AgentLink, _ ok: (AgentState) -> Bool) async -> Bool {
    for _ in 0..<100 {
        if ok(link.state) { return true }
        try? await Task.sleep(for: .milliseconds(50))
    }
    return false
}

@Suite(.serialized) struct AgentLinkOverASocket {
    @Test func subscribesAndShowsTheLiveStatus() async throws {
        let agent = try FakeAgent(answers())
        try await Task.sleep(for: .milliseconds(200))
        let link = AgentLink(socketPath: agent.path, verificacion: .rutaYUsuario)
        link.start()
        defer { link.stop() }
        #expect(await waitFor(link) { $0 == .live(ready) })
    }

    @Test func noAgentIsSaidHonestly() async {
        let link = AgentLink(socketPath: NSTemporaryDirectory() + "fogar-none-\(UUID().uuidString.prefix(8)).sock")
        link.start()
        defer { link.stop() }
        #expect(await waitFor(link) { $0 == .notRunning })
    }

    @Test func versionMismatchNamesTheAgentVersion() async throws {
        let agent = try FakeAgent(answers(version: ipcVersion + 1))
        try await Task.sleep(for: .milliseconds(200))
        let link = AgentLink(socketPath: agent.path, verificacion: .rutaYUsuario)
        link.start()
        defer { link.stop() }
        #expect(await waitFor(link) { $0 == .versionMismatch(agent: ipcVersion + 1) })
        #expect(Textos.titular(link.state).titulo == "Actualiza Fogar Desktop")
    }

    @Test func commandsSkipEventsAndMapFailures() async throws {
        let agent = try FakeAgent(answers(login: .loginFailed(WireLoginFailed(reason: .invalidCredentials))))
        try await Task.sleep(for: .milliseconds(200))
        let link = AgentLink(socketPath: agent.path, verificacion: .rutaYUsuario)
        #expect(try await link.discover().isEmpty)
        #expect(try await link.login(address: "https://casa.ts.net", username: "sito", password: "mala") == .failed(.invalidCredentials))
        #expect(try await link.logout())
        try await link.reconnect()
    }

    @Test func loginReturnsTheSignedInGateway() async throws {
        let agent = try FakeAgent(answers())
        try await Task.sleep(for: .milliseconds(200))
        let link = AgentLink(socketPath: agent.path, verificacion: .rutaYUsuario)
        #expect(try await link.login(address: "https://casa.ts.net", username: "sito", password: "fogar") == .loggedIn(ready.gateway!))
    }

    /// The account is gone from this Mac either way; the person learns the gateway did not hear.
    @Test func logoutWithTheGatewayDownSaysSo() async throws {
        let agent = try FakeAgent(answers(logout: .loggedOut(WireLoggedOut(gateway_reached: false))))
        try await Task.sleep(for: .milliseconds(200))
        let link = AgentLink(socketPath: agent.path, verificacion: .rutaYUsuario)
        let alcanzado = try await link.logout()
        #expect(!alcanzado)
        #expect(Textos.sesionCerrada(gatewayAlcanzado: alcanzado)
                == "Sesión cerrada en este Mac. El gateway no contestó: retira este aparato desde la web si hace falta.")
    }

    /// The signed agent could not delete the Keychain item: say that, not the signer sentence.
    @Test func logoutKeychainFailureIsSaid() async throws {
        let agent = try FakeAgent(answers(logout: .refused(WireRefused(reason: "could not delete the account"))))
        try await Task.sleep(for: .milliseconds(200))
        let link = AgentLink(socketPath: agent.path, verificacion: .rutaYUsuario)
        await #expect(throws: CommandError.sessionNotDeleted) { _ = try await link.logout() }
        #expect(Textos.fallo(CommandError.sessionNotDeleted) == "No se pudo borrar la sesión del Llavero. Vuelve a intentarlo.")
    }

    @Test func refusedLoginIsTheSignerSentence() async throws {
        let agent = try FakeAgent(answers(login: .refused(WireRefused(reason: "peer not allowed"))))
        try await Task.sleep(for: .milliseconds(200))
        let link = AgentLink(socketPath: agent.path, verificacion: .rutaYUsuario)
        await #expect(throws: CommandError.refused) { _ = try await link.login(address: "x", username: "sito", password: "fogar") }
    }
}

extension AgentLinkOverASocket {
    /// Today's core refuses `subscribe` («not implemented yet»): an honest state, and no 2 Hz
    /// reconnect loop (review finding).
    @Test func refusedSubscribeIsAnOlderAgentNotALoop() async throws {
        let agent = try FakeAgent { req in
            if case .hello = req { return [.hello(WireHello(version: ipcVersion))] }
            return [.refused(WireRefused(reason: "not implemented yet"))]
        }
        try await Task.sleep(for: .milliseconds(200))
        let link = AgentLink(socketPath: agent.path, verificacion: .rutaYUsuario)
        link.start()
        defer { link.stop() }
        #expect(await waitFor(link) { $0 == .noLiveStatus })
        try await Task.sleep(for: .seconds(2))
        #expect(agent.accepted == 1)
    }

    /// The agent drops the subscription (restart): the app says so, then comes back on its own.
    @Test func reconnectsAfterTheAgentDropsTheLink() async throws {
        let agent = try FakeAgent(dropAfterSubscribe: .milliseconds(300), answers())
        try await Task.sleep(for: .milliseconds(200))
        let link = AgentLink(socketPath: agent.path, verificacion: .rutaYUsuario)
        link.start()
        defer { link.stop() }
        #expect(await waitFor(link) { $0 == .live(ready) })
        #expect(await waitFor(link) { $0 == .notRunning })
        #expect(await waitFor(link) { $0 == .live(ready) })
        #expect(agent.accepted >= 2)
    }

    @Test func undecodableEventIsSaidNotAgentDown() async throws {
        FakeAgent.rawAfterSubscribe = #"{"type":"event","data":{"phase":"dancing"}}"#
        defer { FakeAgent.rawAfterSubscribe = nil }
        let agent = try FakeAgent { req in
            if case .hello = req { return [.hello(WireHello(version: ipcVersion))] }
            return [.subscribed]
        }
        try await Task.sleep(for: .milliseconds(200))
        let link = AgentLink(socketPath: agent.path, verificacion: .rutaYUsuario)
        link.start()
        defer { link.stop() }
        #expect(await waitFor(link) { $0 == .unintelligible })
    }
}

@Suite struct Copy {
    @Test func everyLoginFailureHasItsOwnSentence() {
        let all: [WireLoginFailure] = [.badAddress, .needsSecureAddress, .badCredentialsInput, .unreachable,
                                       .notAGateway, .invalidCredentials, .insecureTransport, .tooManyAttempts,
                                       .gatewayError, .storageFailed, .alreadySignedIn, .busy]
        let sentences = Set(all.map(Textos.fallo))
        #expect(sentences.count == all.count)
        #expect(Textos.fallo(.needsSecureAddress) == "Esa dirección es de tu red de casa: la contraseña viajaría sin cifrar. Usa la dirección https:// del gateway o la de Tailscale (termina en .ts.net).")
        #expect(Textos.fallo(.invalidCredentials) == "Usuario o contraseña incorrectos.")
        #expect(Textos.fallo(.insecureTransport) == "El gateway no acepta la contraseña por esta vía. Usa su dirección https:// o la de Tailscale.")
        #expect(Textos.fallo(.tooManyAttempts) == "Demasiados intentos. Espera un poco y vuelve a probar.")
        #expect(Textos.fallo(.badCredentialsInput) == "Escribe tu usuario y tu contraseña.")
        #expect(Textos.fallo(.storageFailed) == "No se pudo guardar la sesión en el Llavero.")
    }

    /// The status names the person and the home; never «como » with an unknown person.
    @Test func readyNamesThePersonAndTheHome() {
        let conPersona = WireGateway(address: "https://casa.ts.net", name: "green-house", version: "5.0.0", client_name: "Mac", user: "Sito")
        let s = WireStatus(link: "ready", phase: .ready, gateway: conPersona, sessions: 1, turns_completed: 0,
                           boot_mode: "normal", peer_check: .sameSigner, transcriber: .up, tts: .up)
        let t = Textos.titular(.live(s))
        #expect(t.titulo == "Conectado a green-house")
        #expect(t.sub.contains("Sito"))
        #expect(!Textos.titular(.live(ready)).sub.contains("como"))
    }

    @Test func everyPhaseHasAHeadline() {
        for phase: WirePhase in [.notPaired, .connecting, .ready, .reconnecting, .parked, .stopping] {
            let s = WireStatus(link: phase.rawValue, phase: phase, gateway: ready.gateway, sessions: 0,
                               turns_completed: 0, boot_mode: "normal", peer_check: .uidOnly, transcriber: .unknown, tts: .unknown)
            #expect(!Textos.titular(.live(s)).titulo.isEmpty)
        }
        #expect(Textos.titular(.versionMismatch(agent: 1)).titulo == "Actualiza el agente de Fogar")
    }

    @Test func upgradeParkSaysToUpdate() {
        let s = WireStatus(link: "parked_upgrade_client", phase: .parked, gateway: ready.gateway, sessions: 0,
                           turns_completed: 0, boot_mode: "normal", peer_check: .sameSigner, transcriber: .unknown, tts: .unknown)
        #expect(Textos.titular(.live(s)).sub == "El gateway habla una versión nueva. Actualiza Fogar Desktop; si ya lo hiciste, pulsa Reconectar.")
    }

    /// Gateway 5.0 close 4011: the key still works; the admin grants the scope, then Reconectar.
    @Test func missingScopeParkSaysToAskForPermission() {
        let s = WireStatus(link: "parked_missing_scope", phase: .parked, gateway: ready.gateway, sessions: 0,
                           turns_completed: 0, boot_mode: "normal", peer_check: .sameSigner, transcriber: .unknown, tts: .unknown)
        #expect(Textos.titular(.live(s)).sub == "Este Mac no tiene permiso para conversar con Fogar. Pide a quien administra Fogar que se lo dé y pulsa Reconectar.")
    }

    /// The pauses the person can lift say how, and no longer send them to restart the agent.
    @Test func pausesWithReconnectSayReconectar() {
        for link in ["parked_missing_scope", "parked_agent_refused", "parked_upgrade_client"] {
            let s = WireStatus(link: link, phase: .parked, gateway: ready.gateway, sessions: 0,
                               turns_completed: 0, boot_mode: "normal", peer_check: .sameSigner, transcriber: .unknown, tts: .unknown)
            let sub = Textos.titular(.live(s)).sub
            #expect(sub.contains("Reconectar"))
            #expect(!sub.contains("einicia"))
        }
    }
}

nonisolated let notPaired = WireStatus(link: "not_paired", phase: .notPaired, gateway: nil, sessions: 1,
                                       turns_completed: 2, boot_mode: "normal", peer_check: .sameSigner, transcriber: .unknown, tts: .unknown)

/// Revocation (gateway 4200/4010): the agent goes from signed in to `not_paired` by itself.
@Suite struct AccesoRetirado {
    @Test func signedInToNotPairedWithoutAskingIsARevocation() {
        #expect(AgentLink.retirado(antes: .live(ready), despues: .live(notPaired), cerrandoSesion: false, yaRetirado: false))
    }

    @Test func signingOutFromThisAppIsNot() {
        #expect(!AgentLink.retirado(antes: .live(ready), despues: .live(notPaired), cerrandoSesion: true, yaRetirado: false))
    }

    @Test func neverSignedInIsNot() {
        #expect(!AgentLink.retirado(antes: .looking, despues: .live(notPaired), cerrandoSesion: false, yaRetirado: false))
        #expect(!AgentLink.retirado(antes: .notRunning, despues: .live(notPaired), cerrandoSesion: false, yaRetirado: false))
        // Keychain error at boot: `reconnecting` with no gateway, then `not_paired`.
        let sinClave = WireStatus(link: "backoff", phase: .reconnecting, gateway: nil, sessions: 0,
                                  turns_completed: 0, boot_mode: "normal", peer_check: .sameSigner, transcriber: .unknown, tts: .unknown)
        #expect(!AgentLink.retirado(antes: .live(sinClave), despues: .live(notPaired), cerrandoSesion: false, yaRetirado: false))
    }

    @Test func survivesAnAgentRestartAndEndsWhenSignedInAgain() {
        #expect(AgentLink.retirado(antes: .live(notPaired), despues: .notRunning, cerrandoSesion: false, yaRetirado: true))
        #expect(AgentLink.retirado(antes: .notRunning, despues: .live(notPaired), cerrandoSesion: false, yaRetirado: true))
        #expect(!AgentLink.retirado(antes: .live(notPaired), despues: .live(ready), cerrandoSesion: false, yaRetirado: true))
    }

    /// This Mac is never a sign-in candidate (another local user could listen there).
    @Test func thisMacIsNotASignInCandidate() {
        for a in ["http://127.0.0.1:8004", "127.0.0.1", "127.0.0.2:8004", "localhost:8004", "http://LOCALHOST",
                  "http://[::1]:8004", "[::1]", "http://[::ffff:127.0.0.1]:8004"] {
            #expect(esDeEsteMac(a), "\(a)")
        }
        for a in ["http://100.89.15.7:8004", "casa.tail1.ts.net", "https://fogar.example.com", "192.168.1.19:8004",
                  "http://[fd7a:115c:a1e0::1]:8004", "localhost.example.com"] {
            #expect(!esDeEsteMac(a), "\(a)")
        }
    }

    /// A revocation jumps to «Iniciar sesión» even from Ajustes; this app's own logout from
    /// Ajustes stays there (its notice stays on screen).
    @Test func revocationJumpsToSignInFromAnywhere() {
        #expect(Navegacion.irAIniciarSesion(antes: .ready, despues: .notPaired, en: .ajustes, retirado: true))
        #expect(Navegacion.irAIniciarSesion(antes: .ready, despues: .notPaired, en: .estado, retirado: false))
        #expect(!Navegacion.irAIniciarSesion(antes: .ready, despues: .notPaired, en: .ajustes, retirado: false))
        #expect(!Navegacion.irAIniciarSesion(antes: nil, despues: .notPaired, en: .estado, retirado: true))
        #expect(!Navegacion.irAIniciarSesion(antes: .notPaired, despues: .notPaired, en: .estado, retirado: true))
        #expect(!Navegacion.irAIniciarSesion(antes: .notPaired, despues: .ready, en: .estado, retirado: false))
    }

    @Test func theHeadlineSaysAccessWasWithdrawn() {
        #expect(Textos.titular(.live(notPaired), retirado: true).titulo == "Se retiró el acceso a este Mac")
        #expect(Textos.titular(.live(notPaired)).titulo == "Aún no has iniciado sesión en este Mac")
        #expect(Textos.titular(.live(notPaired), retirado: true).sub.contains("inicia sesión"))
    }
}

extension AgentLinkOverASocket {
    @Test func liveRevocationRaisesTheNotice() async throws {
        let agent = try FakeAgent { req in
            switch req {
            case .hello: [.hello(WireHello(version: ipcVersion))]
            case .subscribe: [.subscribed, .event(ready), .event(notPaired)]
            default: [.refused(WireRefused(reason: "x"))]
            }
        }
        try await Task.sleep(for: .milliseconds(200))
        let link = AgentLink(socketPath: agent.path, verificacion: .rutaYUsuario)
        link.start()
        defer { link.stop() }
        #expect(await waitFor(link) { $0 == .live(notPaired) })
        #expect(link.accesoRetirado)
    }
}
