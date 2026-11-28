import FogarWire
import Foundation
import Observation

/// The conversation connection's state. The view itself is `ConversationLink.vista`.
enum EnlaceConversacion: Equatable {
    /// First attempt, or between attempts after the agent went away.
    case buscando
    /// No agent on the socket.
    case sinAgente
    /// The agent answered `refused`: the conversation is only for the app signed like the agent.
    case soloAppFirmada
    /// The agent speaks another version, sends something this app cannot read, or is not provably
    /// our agent (`AgentLink` says which).
    case noDisponible
    /// Subscribed: the agent pushes the latest view on every change.
    case viva
}

/// The app's conversation with the agent (IPC v6): ONE long connection that is subscribed to the
/// open conversation and to the list, and also carries the conversation's requests. It has to be
/// one: the agent only lets the subscribed connection start a take, and a take ends with
/// `discard` when its connection goes. (`request_mic_permission` is the exception: it may wait
/// for the person at the system prompt, so it goes on a single-use connection of its own and
/// never holds up a Cancel.)
///
/// No request ids on the wire: the agent answers a connection's requests in order, and the only
/// frames it sends unasked here are `conversation` and `conversations`. So every other frame
/// answers the oldest request still waiting. A request that gets no answer in time (counted from when its frame was written)
/// closes the connection (the order could no longer be trusted) and every waiting request fails;
/// the loop reconnects.
///
/// Before a byte is sent, the server is authenticated (`VerificacionPar`, ADR-0002 mirrored).
/// The app keeps the view in memory only: never on disk, never in logs.
@Observable
final class ConversationLink {
    private(set) var enlace: EnlaceConversacion = .buscando
    /// The latest view the agent pushed. Kept after the connection drops (shown as stale) and
    /// replaced by the next subscription's first push.
    private(set) var vista: WireConversation?
    /// The conversations, newest first, as the agent last pushed them (kept like `vista`).
    private(set) var lista: [WireSummary] = []
    /// Called on every new view (the model takes the draft from here).
    @ObservationIgnored var alCambiar: (WireConversation) -> Void = { _ in }
    /// Called when the connection drops (any take on it ended with `discard` in the agent).
    @ObservationIgnored var alPerder: () -> Void = {}

    @ObservationIgnored let socketPath: String
    @ObservationIgnored let verificacion: VerificacionPar
    @ObservationIgnored private var loop: Task<Void, Never>?
    @ObservationIgnored private var nap: Task<Void, Never>?
    @ObservationIgnored private var conexion: FramedConnection?
    /// Requests sent on `conexion` and not answered yet, oldest first.
    @ObservationIgnored private var pendientes: [(id: UInt64, k: CheckedContinuation<WireResponse, any Error>)] = []
    /// Each waiting request's deadline, cancelled when its answer arrives.
    @ObservationIgnored private var plazos: [UInt64: Task<Void, Never>] = [:]
    @ObservationIgnored private var siguienteId: UInt64 = 0
    /// Sends go out one after another, in the order they were asked for (the answers' order).
    @ObservationIgnored private var cola: Task<Void, Never>?

    init(socketPath: String = AgentLink.defaultSocketPath, verificacion: VerificacionPar = AgentLink.verificacionPorDefecto) {
        self.socketPath = socketPath
        self.verificacion = verificacion
    }

    func start() {
        guard loop == nil else { return }
        loop = Task { await run() }
    }

    func stop() {
        loop?.cancel()
        loop = nil
        conexion?.cancel()
    }

    /// Cuts the backoff short (the agent came back, the person pressed «Reintentar»).
    func retryNow() {
        nap?.cancel()
    }

    // MARK: Subscription

    private func run() async {
        var backoff = Duration.milliseconds(500)
        while !Task.isCancelled {
            var permanente = false
            var estuvoViva = false
            do {
                let v = verificacion
                let c = try await FramedConnection.connect(path: socketPath, verificacion: v)
                defer { c.cancel() }
                let primera = try await withDeadline(.seconds(5)) { () -> WireConversation? in
                    try await Self.saludar(c, v)
                    try await c.send(WireCodec.encode(WireRequest.subscribeConversation))
                    // `conversation_subscribed`, then the first view.
                    switch try WireCodec.decodeResponse(await c.receive()) {
                    case .conversationSubscribed: break
                    case .refused: throw CommandError.refused // not the signed app (strictly same signer)
                    default: throw CommandError.unexpected
                    }
                    // The list on the same connection: `list_subscribed`, then the first list (read
                    // by the loop below). The latest view pushed meanwhile is kept.
                    try await c.send(WireCodec.encode(WireRequest.listConversations))
                    var ultima: WireConversation?
                    while true {
                        switch try WireCodec.decodeResponseTolerante(await c.receive()) {
                        case .listSubscribed: return ultima
                        case .conversation(let vista): ultima = vista
                        case .refused: throw CommandError.refused
                        default: throw CommandError.unexpected
                        }
                    }
                }
                if let primera { recibir(.conversation(primera)) }
                conexion = c
                enlace = .viva
                estuvoViva = true
                backoff = .milliseconds(500)
                defer { soltar(c) }
                while !Task.isCancelled {
                    let respuesta = try WireCodec.decodeResponseTolerante(await c.receive())
                    recibir(respuesta)
                }
            } catch CommandError.refused {
                enlace = .soloAppFirmada; permanente = true
            } catch CommandError.versionMismatch, CommandError.unexpected, is DecodingError, LinkError.frame, LinkError.notOurAgent {
                enlace = .noDisponible; permanente = true
            } catch {
                // Lost after it worked (agent restart, a request with no answer): look again soon.
                enlace = estuvoViva ? .buscando : .sinAgente
            }
            await dormir(permanente ? .seconds(60) : backoff)
            backoff = min(backoff * 2, .seconds(8))
        }
    }

    private func recibir(_ r: WireResponse) {
        switch r {
        case .conversation(let v):
            vista = v
            alCambiar(v)
        case .conversations(let l):
            lista = l.conversations
        case .event, .status, .subscribed, .conversationSubscribed, .listSubscribed:
            break // nothing asked for these on this connection
        default:
            // The answer to the oldest request waiting. An answer nobody waits for (one that came
            // after its request timed out cannot: that closes the connection) is ignored.
            guard !pendientes.isEmpty else { return }
            let p = pendientes.removeFirst()
            plazos.removeValue(forKey: p.id)?.cancel()
            p.k.resume(returning: r)
        }
    }

    /// The connection ended: nothing waiting on it will be answered.
    private func soltar(_ c: FramedConnection) {
        c.cancel()
        if conexion === c { conexion = nil }
        let fallidos = pendientes
        pendientes.removeAll()
        for (_, t) in plazos { t.cancel() }
        plazos.removeAll()
        for p in fallidos { p.k.resume(throwing: CommandError.noAnswer) }
        if enlace == .viva { enlace = .buscando }
        alPerder()
    }

    private func dormir(_ d: Duration) async {
        let t = Task<Void, Never> { try? await Task.sleep(for: d) }
        nap = t
        await withTaskCancellationHandler { await t.value } onCancel: { t.cancel() }
        nap = nil
    }

    /// `hello`, then the process that answered is checked again before anything else is sent.
    nonisolated private static func saludar(_ c: FramedConnection, _ v: VerificacionPar) async throws {
        try await c.send(WireCodec.encode(WireRequest.hello(WireHello(version: ipcVersion))))
        let respuesta = try WireCodec.decodeResponse(await c.receive())
        try await c.reverificar(v)
        switch respuesta {
        case .hello(let h) where h.version == ipcVersion: return
        case .hello(let h): throw CommandError.versionMismatch(agent: h.version)
        case .refused: throw CommandError.refused
        default: throw CommandError.unexpected
        }
    }

    // MARK: Requests

    /// Sends `request` on the conversation connection and waits for its answer (`refused` is
    /// returned as an answer: what it means depends on the request). `plazo` counts from when the
    /// frame was written, not from when it was queued behind earlier requests.
    func pedir(_ request: WireRequest, plazo: Duration = .seconds(5)) async throws(CommandError) -> WireResponse {
        guard let c = conexion else { throw .agentNotRunning }
        let cuerpo: Data
        do { cuerpo = try WireCodec.encode(request) } catch { throw .unexpected }
        siguienteId += 1
        let id = siguienteId
        do {
            return try await withCheckedThrowingContinuation { (k: CheckedContinuation<WireResponse, any Error>) in
                pendientes.append((id, k))
                let anterior = cola
                cola = Task {
                    await anterior?.value
                    // Answered or failed meanwhile (the connection dropped): nothing to write.
                    guard pendientes.contains(where: { $0.id == id }) else { return }
                    do {
                        try await c.send(cuerpo)
                    } catch {
                        c.cancel()
                        return
                    }
                    guard pendientes.contains(where: { $0.id == id }) else { return }
                    // The deadline: past it the answers' order is lost, so the connection goes.
                    plazos[id] = Task { [weak self] in
                        try? await Task.sleep(for: plazo)
                        guard !Task.isCancelled, let self, self.pendientes.contains(where: { $0.id == id }) else { return }
                        c.cancel()
                    }
                }
            }
        } catch let e as CommandError {
            throw e
        } catch {
            throw .noAnswer
        }
    }

    /// One request on a connection of its own (`hello` → request → answer → close), for what may
    /// wait on the person (the microphone permission prompt). Does not need the subscription.
    /// `plazo` counts from when the request was written.
    func pedirAparte(_ request: WireRequest, plazo: Duration) async throws(CommandError) -> WireResponse {
        let path = socketPath
        let v = verificacion
        do {
            let c = try await FramedConnection.connect(path: path, verificacion: v)
            defer { c.cancel() }
            try await withDeadline(.seconds(5)) {
                try await Self.saludar(c, v)
                try await c.send(WireCodec.encode(request))
            }
            return try await withDeadline(plazo) {
                while true {
                    switch try WireCodec.decodeResponseTolerante(await c.receive()) {
                    case .event, .status, .conversation, .conversations: continue // never the answer
                    case let answer: return answer
                    }
                }
            }
        } catch let e as CommandError {
            throw e
        } catch LinkError.unavailable {
            throw .agentNotRunning
        } catch LinkError.notOurAgent {
            throw .notOurAgent
        } catch {
            throw .noAnswer
        }
    }
}
