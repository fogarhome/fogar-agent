import FogarWire
import Foundation
import Network
import Testing
@testable import FogarDesktop

/// A scripted agent on a real Unix socket: answers each request with `respond`.
final class FakeAgent: @unchecked Sendable {
    let path = NSTemporaryDirectory() + "fogar-t-\(UUID().uuidString.prefix(8)).sock"
    private var listener: NWListener?

    init(_ respond: @escaping @Sendable (WireRequest) -> [WireResponse]) throws {
        unlink(path)
        let params = NWParameters.tcp
        params.requiredLocalEndpoint = .unix(path: path)
        let l = try NWListener(using: params)
        l.newConnectionHandler = { conn in
            Task {
                let c = FramedConnection(accepted: conn)
                do {
                    try await c.start()
                    while true {
                        let req = try WireCodec.decodeRequest(try await c.receive())
                        for r in respond(req) { try await c.send(WireCodec.encode(r)) }
                    }
                } catch {}
            }
        }
        l.start(queue: .global())
        listener = l
    }

    deinit {
        listener?.cancel()
        unlink(path)
    }
}

nonisolated let ready = WireStatus(link: "ready", phase: .ready,
                       gateway: WireGateway(address: "http://green-house:8004", name: "green-house", version: "1.29.0", client_name: "Mac"),
                       sessions: 1, turns_completed: 2, boot_mode: "normal", peer_check: .sameSigner)

func answers(version: UInt32 = 2, pair: WireResponse = .unpaired) -> @Sendable (WireRequest) -> [WireResponse] {
    { req in
        switch req {
        case .hello: [.hello(WireHello(version: version))]
        case .subscribe: [.subscribed, .event(ready)]
        case .discover: [.event(ready), .discovered(WireDiscovered(gateways: []))] // a stray event must not be the answer
        case .pair: [pair]
        case .unpair: [.unpaired]
        case .status: [.status(ready)]
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
        let link = AgentLink(socketPath: agent.path)
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
        let agent = try FakeAgent(answers(version: 3))
        try await Task.sleep(for: .milliseconds(200))
        let link = AgentLink(socketPath: agent.path)
        link.start()
        defer { link.stop() }
        #expect(await waitFor(link) { $0 == .versionMismatch(agent: 3) })
        #expect(Textos.titular(link.state).titulo == "Actualiza Fogar Desktop")
    }

    @Test func commandsSkipEventsAndMapFailures() async throws {
        let agent = try FakeAgent(answers(pair: .pairFailed(WirePairFailed(reason: .invalidCode))))
        try await Task.sleep(for: .milliseconds(200))
        let link = AgentLink(socketPath: agent.path)
        #expect(try await link.discover().isEmpty)
        #expect(try await link.pair(address: "green-house", code: "111111") == .failed(.invalidCode))
        try await link.unpair()
    }

    @Test func refusedPairIsTheSignerSentence() async throws {
        let agent = try FakeAgent(answers(pair: .refused(WireRefused(reason: "peer not allowed"))))
        try await Task.sleep(for: .milliseconds(200))
        let link = AgentLink(socketPath: agent.path)
        await #expect(throws: CommandError.refused) { _ = try await link.pair(address: "x", code: "482913") }
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
                               turns_completed: 0, boot_mode: "normal", peer_check: .uidOnly)
            #expect(!Textos.titular(.live(s)).titulo.isEmpty)
        }
        #expect(Textos.titular(.versionMismatch(agent: 1)).titulo == "Actualiza el agente de Fogar")
    }
}
