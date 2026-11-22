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
                       gateway: WireGateway(address: "http://green-house:8004", name: "green-house", version: "1.29.0", client_name: "Mac"),
                       sessions: 1, turns_completed: 2, boot_mode: "normal", peer_check: .sameSigner, transcriber: .unknown, tts: .unknown)

func answers(version: UInt32 = ipcVersion, pair: WireResponse = .unpaired) -> @Sendable (WireRequest) -> [WireResponse] {
    { req in
        switch req {
        case .hello: [.hello(WireHello(version: version))]
        case .subscribe: [.subscribed, .event(ready)]
        case .discover: [.event(ready), .discovered(WireDiscovered(gateways: []))] // a stray event must not be the answer
        case .pair: [pair]
        case .unpair: [.unpaired]
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
        let agent = try FakeAgent(answers(pair: .pairFailed(WirePairFailed(reason: .invalidCode))))
        try await Task.sleep(for: .milliseconds(200))
        let link = AgentLink(socketPath: agent.path, verificacion: .rutaYUsuario)
        #expect(try await link.discover().isEmpty)
        #expect(try await link.pair(address: "green-house", code: "111111") == .failed(.invalidCode))
        try await link.unpair()
        try await link.reconnect()
    }

    @Test func refusedPairIsTheSignerSentence() async throws {
        let agent = try FakeAgent(answers(pair: .refused(WireRefused(reason: "peer not allowed"))))
        try await Task.sleep(for: .milliseconds(200))
        let link = AgentLink(socketPath: agent.path, verificacion: .rutaYUsuario)
        await #expect(throws: CommandError.refused) { _ = try await link.pair(address: "x", code: "482913") }
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
    @Test func everyPairFailureHasItsOwnSentence() {
        let all: [WirePairFailure] = [.badAddress, .badCode, .unreachable, .notAGateway, .invalidCode,
                                      .gatewayError, .storageFailed, .alreadyPaired, .busy]
        let sentences = Set(all.map(Textos.fallo))
        #expect(sentences.count == all.count)
        #expect(Textos.fallo(.invalidCode) == "Ese código no vale. Si estás seguro, pide uno nuevo.")
    }

    @Test func everyPhaseHasAHeadline() {
        for phase: WirePhase in [.notPaired, .connecting, .ready, .reconnecting, .parked, .stopping] {
            let s = WireStatus(link: phase.rawValue, phase: phase, gateway: ready.gateway, sessions: 0,
                               turns_completed: 0, boot_mode: "normal", peer_check: .uidOnly, transcriber: .unknown, tts: .unknown)
            #expect(!Textos.titular(.live(s)).titulo.isEmpty)
        }
        #expect(Textos.titular(.versionMismatch(agent: 1)).titulo == "Actualiza el agente de Fogar")
    }

    /// The two pauses the person can lift say how, and no longer send them to restart the agent.
    @Test func pausesWithReconnectSayReconectar() {
        for link in ["parked_conversation_taken", "parked_agent_refused"] {
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

/// Revocation (gateway 4200/4010): the agent goes from paired to `not_paired` by itself.
@Suite struct AccesoRetirado {
    @Test func pairedToNotPairedWithoutAskingIsARevocation() {
        #expect(AgentLink.retirado(antes: .live(ready), despues: .live(notPaired), desemparejando: false, yaRetirado: false))
    }

    @Test func unpairingFromThisAppIsNot() {
        #expect(!AgentLink.retirado(antes: .live(ready), despues: .live(notPaired), desemparejando: true, yaRetirado: false))
    }

    @Test func neverPairedIsNot() {
        #expect(!AgentLink.retirado(antes: .looking, despues: .live(notPaired), desemparejando: false, yaRetirado: false))
        #expect(!AgentLink.retirado(antes: .notRunning, despues: .live(notPaired), desemparejando: false, yaRetirado: false))
        // Keychain error at boot: `reconnecting` with no gateway, then `not_paired`.
        let sinClave = WireStatus(link: "backoff", phase: .reconnecting, gateway: nil, sessions: 0,
                                  turns_completed: 0, boot_mode: "normal", peer_check: .sameSigner, transcriber: .unknown, tts: .unknown)
        #expect(!AgentLink.retirado(antes: .live(sinClave), despues: .live(notPaired), desemparejando: false, yaRetirado: false))
    }

    @Test func survivesAnAgentRestartAndEndsWhenPairedAgain() {
        #expect(AgentLink.retirado(antes: .live(notPaired), despues: .notRunning, desemparejando: false, yaRetirado: true))
        #expect(AgentLink.retirado(antes: .notRunning, despues: .live(notPaired), desemparejando: false, yaRetirado: true))
        #expect(!AgentLink.retirado(antes: .live(notPaired), despues: .live(ready), desemparejando: false, yaRetirado: true))
    }

    @Test func theHeadlineSaysAccessWasWithdrawn() {
        #expect(Textos.titular(.live(notPaired), retirado: true).titulo == "Se retiró el acceso a este Mac")
        #expect(Textos.titular(.live(notPaired)).titulo == "Este Mac aún no está emparejado")
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
