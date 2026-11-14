// A stand-in for fogar-agent's UI socket, for developing and screenshotting Fogar Desktop without
// the Rust agent. It speaks the same frames (FogarWire) and plays scripted states. Synthetic data.
//
//   swift run fogar-standin [--socket PATH] [--phase not_paired|connecting|ready|reconnecting|
//       parked_repair|parked_agent_refused|parked_client_error|stopping] [--version N]
//       [--gateways 0|1|2] [--refuse-pair] [--peer uid_only|same_signer] [--boot safe] [--old]
//
// Pairing codes decide the outcome: 482913 pairs; 111111 invalid_code, 222222 unreachable,
// 333333 not_a_gateway, 444444 gateway_error, 555555 storage_failed, 666666 busy,
// 777777 bad_address; any other 6 digits invalid_code. Pairing while paired -> already_paired.
// Typed on stdin: a phase name (above) switches the live state; `quit` stops.
import FogarWire
import Foundation
import Network

struct Options {
    var socket = AgentSocket.path(allowOverride: true)
    var phase = "not_paired"
    var version: UInt32 = ipcVersion
    var gateways = 1
    var refusePair = false
    var peer = WirePeerCheck.sameSigner
    var boot = "normal"
    /// Plays today's pre-S1 core: `subscribe`, `discover`, `pair`, `unpair` answered `refused`.
    var old = false

    init(_ args: [String]) {
        var it = args.dropFirst().makeIterator()
        while let a = it.next() {
            switch a {
            case "--socket": socket = it.next() ?? socket
            case "--phase": phase = it.next() ?? phase
            case "--version": version = UInt32(it.next() ?? "") ?? version
            case "--gateways": gateways = Int(it.next() ?? "") ?? gateways
            case "--refuse-pair": refusePair = true
            case "--peer": peer = WirePeerCheck(rawValue: it.next() ?? "") ?? peer
            case "--boot": boot = it.next() ?? boot
            case "--old": old = true
            default: fail("unknown argument \(a)")
            }
        }
    }
}

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data("fogar-standin: \(message)\n".utf8))
    exit(2)
}

let greenHouse = WireGateway(
    address: "http://green-house:8004", name: "green-house", version: "1.29.0", client_name: "Mac de Sito")

/// The agent's state and its subscribers. Phases follow `LinkState::label` / `LinkPhase`.
actor StandIn {
    let options: Options
    private var link: String
    private var gateway: WireGateway?
    private var subscribers: [ObjectIdentifier: FramedConnection] = [:]
    private var pairing = false

    init(_ options: Options) {
        self.options = options
        link = options.phase
        gateway = options.phase == "not_paired" ? nil : greenHouse
    }

    var status: WireStatus {
        let phase: WirePhase = switch link {
        case "ready": .ready
        case "connecting", "starting": .connecting
        case "backoff", "reconnecting": .reconnecting
        case "not_paired": .notPaired
        case "stopping", "shutting_down", "stopped": .stopping
        default: .parked
        }
        return WireStatus(
            link: link == "reconnecting" ? "backoff" : link, phase: phase, gateway: gateway,
            sessions: phase == .ready ? 3 : 0, turns_completed: 7,
            boot_mode: options.boot, peer_check: options.peer)
    }

    func set(link: String) async {
        self.link = link
        if link == "not_paired" { gateway = nil } else if gateway == nil { gateway = greenHouse }
        await broadcast()
    }

    func subscribe(_ c: FramedConnection) { subscribers[ObjectIdentifier(c)] = c }
    func drop(_ c: FramedConnection) { subscribers[ObjectIdentifier(c)] = nil }

    private func broadcast() async {
        let event = WireResponse.event(status)
        for (id, c) in subscribers {
            do { try await c.send(WireCodec.encode(event)) } catch { subscribers[id] = nil }
        }
    }

    func answer(_ request: WireRequest) async -> WireResponse {
        if options.old {
            switch request {
            case .hello, .status: break
            default: return .refused(WireRefused(reason: "not implemented yet"))
            }
        }
        switch request {
        case .hello: return .hello(WireHello(version: options.version))
        case .status: return .status(status)
        case .subscribe: return .subscribed
        case .discover:
            try? await Task.sleep(for: .milliseconds(800))
            let all = [
                WireCandidate(address: "http://green-house:8004", name: "green-house", version: "1.29.0"),
                WireCandidate(address: "http://100.101.7.12:8004", name: "estudio", version: "1.28.2"),
            ]
            return .discovered(WireDiscovered(gateways: Array(all.prefix(max(0, options.gateways)))))
        case .pair(let p):
            // Like the core: pair/unpair only for a verified same-signer peer.
            if options.refusePair || options.peer == .uidOnly { return .refused(WireRefused(reason: "peer not allowed to pair")) }
            if pairing { return .pairFailed(WirePairFailed(reason: .busy)) }
            if link != "not_paired" { return .pairFailed(WirePairFailed(reason: .alreadyPaired)) }
            guard let code = parsePairCode(p.code) else { return .pairFailed(WirePairFailed(reason: .badCode)) }
            pairing = true
            try? await Task.sleep(for: .milliseconds(1200))
            pairing = false
            let failure: WirePairFailure? = switch code {
            case "482913": nil
            case "222222": .unreachable
            case "333333": .notAGateway
            case "444444": .gatewayError
            case "555555": .storageFailed
            case "666666": .busy
            case "777777": .badAddress
            default: .invalidCode
            }
            if let failure { return .pairFailed(WirePairFailed(reason: failure)) }
            let paired = WireGateway(address: greenHouse.address, name: greenHouse.name,
                                     version: greenHouse.version, client_name: greenHouse.client_name)
            gateway = paired
            await set(link: "connecting")
            Task {
                try? await Task.sleep(for: .milliseconds(1500))
                await self.set(link: "ready")
            }
            return .paired(paired)
        case .unpair:
            if options.refusePair || options.peer == .uidOnly { return .refused(WireRefused(reason: "peer not allowed to unpair")) }
            await set(link: "not_paired")
            return .unpaired
        }
    }
}

func serve(_ c: FramedConnection, _ agent: StandIn) async {
    defer { c.cancel(); Task { await agent.drop(c) } }
    do {
        try await c.start()
        var saludado = false
        while true {
            let request = try WireCodec.decodeRequest(try await c.receive())
            // Like the real agent: `hello` first, or the connection is dropped.
            if case .hello = request { saludado = true } else if !saludado { return }
            let response = await agent.answer(request)
            try await c.send(WireCodec.encode(response))
            if case .subscribe = request, case .subscribed = response {
                await agent.subscribe(c)
                try await c.send(WireCodec.encode(WireResponse.event(await agent.status)))
            }
        }
    } catch {
        // Client gone or sent garbage: drop it, like the real agent.
    }
}

let options = Options(CommandLine.arguments)
let agent = StandIn(options)
let dir = (options.socket as NSString).deletingLastPathComponent
do {
    try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true,
                                            attributes: [.posixPermissions: 0o700])
} catch { fail("cannot create \(dir): \(error)") }
unlink(options.socket)

let parameters = NWParameters.tcp
parameters.requiredLocalEndpoint = .unix(path: options.socket)
guard let listener = try? NWListener(using: parameters) else { fail("cannot listen on \(options.socket)") }
listener.newConnectionHandler = { connection in
    Task { await serve(FramedConnection(accepted: connection), agent) }
}
listener.stateUpdateHandler = { state in
    switch state {
    case .ready: print("fogar-standin: listening on \(options.socket) (phase \(options.phase), ipc v\(options.version))")
    case .failed(let e): fail("listener failed: \(e)")
    default: break
    }
}
listener.start(queue: .main)
// Ctrl-C: the socket file is left behind; the next start unlinks it before listening.
signal(SIGINT, SIG_DFL)

Task.detached {
    while let line = readLine() {
        let word = line.trimmingCharacters(in: .whitespaces)
        if word == "quit" { unlink(options.socket); exit(0) }
        if !word.isEmpty { await agent.set(link: word); print("phase -> \(word)") }
    }
}
dispatchMain()
