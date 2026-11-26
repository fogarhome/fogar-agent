// A stand-in for fogar-agent's UI socket, for developing and screenshotting Fogar Desktop without
// the Rust agent. It speaks the same frames (FogarWire) and plays scripted states. Synthetic data.
//
//   swift run fogar-standin [--socket PATH (default /tmp/fogar-standin/agent.sock)] [--phase not_paired|connecting|ready|reconnecting|
//       parked_repair|parked_agent_refused|parked_missing_scope|parked_upgrade_client|parked_client_error|stopping]
//       [--version N]
//       [--gateways 0|1|2] [--refuse-pair] [--peer uid_only|same_signer] [--boot safe] [--old]
//       [--mic granted|denied|not_determined|restricted] [--mic-deny] [--transcriber up|down|degraded]
//       [--tts up|down] [--mic-prompt-secs N] [--no-mic]
//
// Never the real agent's socket: it refuses that path. The app trusts this unsigned stand-in only
// in a debug build started with FOGAR_AGENT_UNSIGNED=1 (see the README).
//
// Pairing codes decide the outcome: 482913 pairs; 111111 invalid_code, 222222 unreachable,
// 333333 not_a_gateway, 444444 gateway_error, 555555 storage_failed, 666666 busy,
// 777777 bad_address; any other 6 digits invalid_code. Pairing while paired -> already_paired.
// Typed on stdin: a phase name (above) switches the live state; `quit` stops.
//
// Conversation (IPC v4, only for a `same_signer` peer, like the core): typed text gets a scripted
// reply (preparing, thinking, a tool, then the text, with audio that plays only after the app's
// `turn_visible`, or is dropped 2 s later). Text with «error» ends in an error, «lento» in a
// timeout. A take plays partials of a scripted phrase, then stops hearing: 5 s later the agent
// cuts it to review (silence_cut). `--mic-deny` answers the permission prompt with «no».
// More stdin words: `push` (Fogar writes on its own), `dia` (a new gateway conversation),
// `mic <state>`, `transcriber <state>`, `tts <state>`, `silencio`, `seguridad`, `frase_perdida`.
import FogarStandinKit
import FogarWire
import Foundation
import Network

struct Options {
    var socket = ProcessInfo.processInfo.environment["FOGAR_AGENT_SOCKET"].flatMap { $0.isEmpty ? nil : $0 }
        ?? "/tmp/fogar-standin/agent.sock"
    var phase = "not_paired"
    var version: UInt32 = ipcVersion
    var gateways = 1
    var refusePair = false
    var peer = WirePeerCheck.sameSigner
    var boot = "normal"
    /// Plays today's pre-S1 core: `subscribe`, `discover`, `pair`, `unpair` answered `refused`.
    var old = false
    var mic = WireMic.notDetermined
    var micDeny = false
    var transcriber = WireService.up
    var tts = WireService.up
    /// How long the «system prompt» takes to answer `request_mic_permission`.
    var micPromptSecs = 0.7
    var noMic = false

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
            case "--mic": mic = WireMic(rawValue: it.next() ?? "") ?? mic
            case "--mic-deny": micDeny = true
            case "--transcriber": transcriber = WireService(rawValue: it.next() ?? "") ?? transcriber
            case "--tts": tts = WireService(rawValue: it.next() ?? "") ?? tts
            case "--mic-prompt-secs": micPromptSecs = Double(it.next() ?? "") ?? micPromptSecs
            case "--no-mic": noMic = true
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
    address: "http://green-house:8004", name: "green-house", version: "1.29.0", client_name: "Mac de Sito", user: "Sito")

/// The agent's state and its subscribers. Phases follow `LinkState::label` / `LinkPhase`.
actor StandIn {
    let options: Options
    private var link: String
    private var gateway: WireGateway?
    private var subscribers: [ObjectIdentifier: FramedConnection] = [:]
    private var pairing = false
    private var sim: ConversacionSimulada
    private var conversacion: [Int: FramedConnection] = [:]

    init(_ options: Options) {
        self.options = options
        link = options.phase
        gateway = options.phase == "not_paired" ? nil : greenHouse
        sim = ConversacionSimulada(mic: options.mic, transcriber: options.transcriber, tts: options.tts,
                                   listo: options.phase == "ready")
        sim.sinMicro = options.noMic
    }

    /// Every task the stand-in starts, kept until done.
    private var tareas: [Int: Task<Void, Never>] = [:]
    private var siguienteTarea = 0

    /// A number for the next task (stored in `tareas`, forgotten by the task when it ends).
    private func registrar() -> Int {
        siguienteTarea += 1
        return siguienteTarea
    }

    private func olvidar(_ id: Int) { tareas[id] = nil }

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
            boot_mode: options.boot, peer_check: options.peer,
            transcriber: phase == .ready ? sim.transcriber : .unknown, tts: phase == .ready ? sim.tts : .unknown)
    }

    func set(link: String) async {
        self.link = link
        if link == "not_paired" { gateway = nil } else if gateway == nil { gateway = greenHouse }
        sim.listo = link == "ready"
        if !sim.listo, let t = sim.turnoAbierto { sim.cerrar(t.turn_id, .closed) }
        await broadcast()
        await difundir()
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
                                     version: greenHouse.version, client_name: greenHouse.client_name,
                                     user: greenHouse.user)
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
        case .login, .logout:
            // Like the core until the account lands (S4 PR 3 Task 2).
            return .refused(WireRefused(reason: "not yet"))
        case .reconnect:
            // Like the core: signed peers only, and only from a pause the person can settle.
            if options.refusePair || options.peer == .uidOnly { return .refused(WireRefused(reason: "peer not allowed to reconnect")) }
            guard link == "parked_missing_scope" || link == "parked_agent_refused" || link == "parked_upgrade_client" else {
                return .refused(WireRefused(reason: "nothing to reconnect"))
            }
            await set(link: "connecting")
            Task {
                try? await Task.sleep(for: .milliseconds(1500))
                await self.set(link: "ready")
            }
            return .reconnecting
        case .subscribeConversation, .sendText, .captureStart, .captureEnd, .cancelTurn,
             .turnVisible, .requestMicPermission:
            // Like the core: the conversation is strictly for the signed app (no dev opt-in).
            return .refused(WireRefused(reason: "needs the signed Fogar app"))
        }
    }

    // MARK: Conversation (IPC v4)

    /// A conversation request from connection `id` (`suscrita`: it sent `subscribe_conversation`).
    /// Like the core, the view that changed is pushed AFTER the answer (`serve` calls `difundir`).
    func conversar(_ request: WireRequest, conexion id: Int, suscrita: Bool) async -> WireResponse {
        guard options.peer == .sameSigner, !options.old else { return soloAppFirmada }
        let respuesta: WireResponse
        switch request {
        case .subscribeConversation:
            return .conversationSubscribed // the view follows from `serve`
        case .sendText(let t):
            respuesta = sim.enviarTexto(t.text)
            if case .textAccepted = respuesta { responder(a: t.text) }
        case .captureStart:
            respuesta = sim.empezarToma(conexion: id, suscrita: suscrita)
            if case .captureStarted = respuesta { escuchar() }
        case .captureEnd(let e):
            let (r, enviar) = sim.terminarToma(e.then, conexion: id)
            respuesta = r
            if let enviar { responder(a: enviar) }
        case .cancelTurn:
            respuesta = sim.cancelar()
        case .turnVisible(let v):
            respuesta = sim.turnoVisible(v.turn_id, suscrita: suscrita)
        case .requestMicPermission:
            try? await Task.sleep(for: .seconds(options.micPromptSecs)) // the system prompt
            respuesta = sim.pedirPermiso(concede: !options.micDeny)
        default:
            return .refused(WireRefused(reason: "unsupported request"))
        }
        return respuesta
    }

    func suscribirConversacion(_ id: Int, _ c: FramedConnection) -> WireResponse {
        conversacion[id] = c
        return .conversation(sim.vista)
    }

    func soltar(_ id: Int) async {
        conversacion[id] = nil
        sim.conexionPerdida(id)
        await difundir()
    }

    /// Latest view to every conversation subscriber (a slow one is dropped, like the core).
    func difundir() async {
        let push = WireResponse.conversation(sim.vista)
        for (id, c) in conversacion {
            do { try await withDeadline(.seconds(2)) { try await c.send(WireCodec.encode(push)) } } catch {
                conversacion[id] = nil
                c.cancel()
                sim.conexionPerdida(id)
            }
        }
    }

    private func cambiar(_ f: (inout ConversacionSimulada) -> Void) async {
        f(&sim)
        await difundir()
    }

    /// The scripted reply to a sent text: preparing, thinking, a tool, then the words.
    private func responder(a texto: String, push: Bool = false) {
        let n = registrar()
        tareas[n] = Task {
            defer { olvidar(n) }
            try? await Task.sleep(for: .milliseconds(push ? 0 : 400))
            var abierto = ""
            await cambiar { abierto = $0.abrirTurno(source: push ? .push : .mine, mine: push ? "" : texto) }
            let id = abierto
            let minusculas = texto.lowercased()
            if !push {
                try? await Task.sleep(for: .milliseconds(600))
                guard sigueAbierto(id) else { return }
                await cambiar { $0.progreso(id, .thinking) }
                try? await Task.sleep(for: .milliseconds(900))
                guard sigueAbierto(id) else { return }
                await cambiar { $0.progreso(id, .tool, herramienta: "ha_call_service") }
                try? await Task.sleep(for: .milliseconds(1000))
            }
            if minusculas.contains("error") { await cambiar { $0.cerrar(id, .error, codigo: "upstream_unavailable") }; return }
            if minusculas.contains("lento") { await cambiar { $0.cerrar(id, .timeout) }; return }
            let respuesta = push ? "Te recuerdo que a las 19:00 tienes que sacar la ropa de la lavadora."
                : minusculas.contains("luz") ? "Hecho: he encendido la luz del salón al 60 %. ¿Quieres que la baje más tarde?"
                : "Aquí tienes una respuesta de prueba del agente sustituto. Llega por trozos, como la del gateway, para ver cómo se va escribiendo en la conversación."
            var primero = true
            for palabra in respuesta.split(separator: " ") {
                guard sigueAbierto(id) else { return }
                await cambiar { $0.texto(id, primero ? String(palabra) : " " + palabra) }
                if primero {
                    primero = false
                    await cambiar { $0.llegaAudio(id) }
                    let m = registrar()
                    tareas[m] = Task {
                        defer { olvidar(m) }
                        try? await Task.sleep(for: .seconds(2))
                        await cambiar { $0.plazoVencido(id) }
                    }
                }
                try? await Task.sleep(for: .milliseconds(70))
            }
            try? await Task.sleep(for: .milliseconds(1200)) // the audio runs a little past the text
            await cambiar { $0.audioTermina(id); $0.cerrar(id, .done) }
        }
    }

    private func sigueAbierto(_ id: String) -> Bool { sim.turno(id)?.outcome == nil }

    /// A take: partials of a scripted phrase, then nothing (the agent cuts to review after 5 s).
    private func escuchar() {
        let n = registrar()
        tareas[n] = Task {
            defer { olvidar(n) }
            let frase = "Enciende la luz del salón y bájala al sesenta por ciento"
            var dicho = ""
            for palabra in frase.split(separator: " ") {
                try? await Task.sleep(for: .milliseconds(380))
                guard sim.activity == .recording else { return }
                dicho += (dicho.isEmpty ? "" : " ") + palabra
                let parcial = dicho
                await cambiar { $0.parcial(parcial) }
            }
            try? await Task.sleep(for: .seconds(5))
            if sim.activity == .recording, sim.live == dicho { await cambiar { $0.cortarToma(.silenceCut) } }
        }
    }

    /// A word typed on the stand-in's stdin. Returns false when it is not a conversation word.
    func orden(_ palabras: [String]) async -> Bool {
        switch (palabras.first ?? "", palabras.dropFirst().first) {
        case ("push", _): responder(a: "", push: true)
        case ("dia", _): await cambiar { $0.nuevoDia("conv-\(UUID().uuidString.prefix(6))") }
        case ("mic", let e?): await cambiar { $0.mic = WireMic(rawValue: e) ?? $0.mic }
        case ("transcriber", let e?): await cambiar { $0.transcriber = WireService(rawValue: e) ?? $0.transcriber }; await broadcast()
        case ("tts", let e?): await cambiar { $0.tts = WireService(rawValue: e) ?? $0.tts }; await broadcast()
        case ("silencio", _): await cambiar { $0.cortarToma(.silenceCut) }
        case ("seguridad", _): await cambiar { $0.cortarToma(.safetyCut) }
        case ("frase_perdida", _): await cambiar { $0.notice = .phraseLost }
        default: return false
        }
        return true
    }
}

func enviar(_ c: FramedConnection, _ r: WireResponse) async throws {
    let cuerpo = try WireCodec.encode(r)
    try await withDeadline(.seconds(2)) { try await c.send(cuerpo) }
}

/// Connection numbers (a take belongs to the connection that opened it).
actor Contador {
    private var n = 0
    func siguiente() -> Int { n += 1; return n }
}
let contador = Contador()

func serve(_ c: FramedConnection, _ agent: StandIn) async {
    let id = await contador.siguiente()
    defer { c.cancel(); Task { await agent.drop(c); await agent.soltar(id) } }
    do {
        try await c.start()
        var saludado = false
        var suscrita = false
        while true {
            let request = try WireCodec.decodeRequest(try await c.receive())
            // Like the real agent: `hello` first, or the connection is dropped.
            if case .hello = request { saludado = true } else if !saludado { return }
            let response: WireResponse
            var conversacion = false
            switch request {
            case .subscribeConversation, .sendText, .captureStart, .captureEnd, .cancelTurn, .turnVisible,
                 .requestMicPermission:
                response = await agent.conversar(request, conexion: id, suscrita: suscrita)
                conversacion = true
            default:
                response = await agent.answer(request)
            }
            // Every write has a deadline: a client that stops reading is dropped, like the core.
            try await enviar(c, response)
            if case .subscribe = request, case .subscribed = response {
                await agent.subscribe(c)
                try await enviar(c, WireResponse.event(await agent.status))
            }
            if case .conversationSubscribed = response {
                suscrita = true
                try await enviar(c, await agent.suscribirConversacion(id, c))
            } else if conversacion {
                // Like the core: the changed view follows the answer.
                await agent.difundir()
            }
        }
    } catch {
        // Client gone or sent garbage: drop it, like the real agent.
    }
}

let options = Options(CommandLine.arguments)
let agent = StandIn(options)

/// The stand-in's socket directory: created private (0700) or, if it exists, ours and a real
/// directory (never a symlink); never the real agent's directory, whatever path leads there.
func prepararSocket(_ socket: String) -> String {
    let dir = (socket as NSString).deletingLastPathComponent
    let uid = getuid()
    var d = stat()
    if lstat(dir, &d) != 0 {
        guard mkdir(dir, 0o700) == 0 else { fail("cannot create \(dir)") }
        guard lstat(dir, &d) == 0 else { fail("cannot read \(dir)") }
    }
    guard d.st_mode & S_IFMT == S_IFDIR, d.st_uid == uid else { fail("\(dir) is not a directory of ours (symlinks refused)") }
    guard chmod(dir, 0o700) == 0 else { fail("cannot make \(dir) private") }
    // Canonical path: no route through symlinks into the real agent's directory.
    guard let real = realpath(dir, nil) else { fail("cannot resolve \(dir)") }
    let canonico = String(cString: real)
    free(real)
    let realAgente = ((AgentSocket.defaultPath as NSString).deletingLastPathComponent as NSString).deletingLastPathComponent
    if canonico.contains("/com.fogar.agent/") || canonico.hasSuffix("/com.fogar.agent") || canonico.hasPrefix(realAgente) {
        fail("refusing a path under the real agent's directory (\(canonico)); use another --socket")
    }
    let ruta = canonico + "/" + (socket as NSString).lastPathComponent
    // Remove a previous stand-in's socket only: a socket file of ours, nothing else.
    var s = stat()
    if lstat(ruta, &s) == 0 {
        guard s.st_mode & S_IFMT == S_IFSOCK, s.st_uid == uid else { fail("\(ruta) exists and is not our socket") }
        unlink(ruta)
    }
    return ruta
}

let rutaSocket = prepararSocket(options.socket)

let parameters = NWParameters.tcp
parameters.requiredLocalEndpoint = .unix(path: rutaSocket)
guard let listener = try? NWListener(using: parameters) else { fail("cannot listen on \(rutaSocket)") }
listener.newConnectionHandler = { connection in
    Task { await serve(FramedConnection(accepted: connection), agent) }
}
listener.stateUpdateHandler = { state in
    switch state {
    case .ready: print("fogar-standin: listening on \(rutaSocket) (phase \(options.phase), ipc v\(options.version))")
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
        if word == "quit" { unlink(rutaSocket); exit(0) }
        if word.isEmpty { continue }
        if await agent.orden(word.split(separator: " ").map(String.init)) { print("conversación -> \(word)"); continue }
        await agent.set(link: word); print("phase -> \(word)")
    }
}
dispatchMain()
