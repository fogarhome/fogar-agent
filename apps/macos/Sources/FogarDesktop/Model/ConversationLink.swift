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
    /// The agent speaks another version or something this app cannot read (`AgentLink` says which).
    case noDisponible
    /// Subscribed: the agent pushes the latest view on every change.
    case viva
}

/// The app's conversation with the agent (IPC v4): ONE long connection that is subscribed to the
/// conversation and also carries every conversation request. It has to be one: the agent only lets
/// the subscribed connection start a take or confirm a turn as visible, and a take ends with
/// `discard` when its connection goes.
///
/// No request ids on the wire: the agent answers a connection's requests in order, and the only
/// frame it sends unasked here is `conversation`. So every other frame answers the oldest request
/// still waiting. A request that gets no answer in time closes the connection (the order could no
/// longer be trusted) and every waiting request fails; the loop reconnects.
///
/// The app keeps the view in memory only: never on disk, never in logs.
@Observable
final class ConversationLink {
    private(set) var enlace: EnlaceConversacion = .buscando
    /// The latest view the agent pushed. Kept after the connection drops (shown as stale) and
    /// replaced by the next subscription's first push.
    private(set) var vista: WireConversation?
    /// Called on every new view (the model confirms visible turns and takes the draft from here).
    @ObservationIgnored var alCambiar: (WireConversation) -> Void = { _ in }
    /// Called when the connection drops (any take on it ended with `discard` in the agent).
    @ObservationIgnored var alPerder: () -> Void = {}

    @ObservationIgnored let socketPath: String
    @ObservationIgnored private var loop: Task<Void, Never>?
    @ObservationIgnored private var nap: Task<Void, Never>?
    @ObservationIgnored private var conexion: FramedConnection?
    /// Requests sent on `conexion` and not answered yet, oldest first.
    @ObservationIgnored private var pendientes: [(id: UInt64, k: CheckedContinuation<WireResponse, any Error>)] = []
    @ObservationIgnored private var siguienteId: UInt64 = 0
    /// Sends go out one after another, in the order they were asked for (the answers' order).
    @ObservationIgnored private var cola: Task<Void, Never>?

    init(socketPath: String = AgentLink.defaultSocketPath) {
        self.socketPath = socketPath
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
                let c = try await FramedConnection.connect(path: socketPath)
                defer { c.cancel() }
                try await withDeadline(.seconds(5)) {
                    try await Self.saludar(c)
                    try await c.send(WireCodec.encode(WireRequest.subscribeConversation))
                    // `conversation_subscribed`, then the first view (read by the loop below).
                    switch try WireCodec.decodeResponse(await c.receive()) {
                    case .conversationSubscribed: break
                    case .refused: throw CommandError.refused // not the signed app (strictly same signer)
                    default: throw CommandError.unexpected
                    }
                }
                conexion = c
                enlace = .viva
                estuvoViva = true
                backoff = .milliseconds(500)
                defer { soltar(c) }
                while !Task.isCancelled {
                    let respuesta = try WireCodec.decodeResponse(await c.receive())
                    recibir(respuesta)
                }
            } catch CommandError.refused {
                enlace = .soloAppFirmada; permanente = true
            } catch CommandError.versionMismatch, CommandError.unexpected, is DecodingError, LinkError.frame {
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
        case .event, .status, .subscribed, .conversationSubscribed:
            break // nothing asked for these on this connection
        default:
            // The answer to the oldest request waiting. An answer nobody waits for (one that came
            // after its request timed out cannot: that closes the connection) is ignored.
            guard !pendientes.isEmpty else { return }
            pendientes.removeFirst().k.resume(returning: r)
        }
    }

    /// The connection ended: nothing waiting on it will be answered.
    private func soltar(_ c: FramedConnection) {
        c.cancel()
        if conexion === c { conexion = nil }
        let fallidos = pendientes
        pendientes.removeAll()
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

    nonisolated private static func saludar(_ c: FramedConnection) async throws {
        try await c.send(WireCodec.encode(WireRequest.hello(WireHello(version: ipcVersion))))
        switch try WireCodec.decodeResponse(await c.receive()) {
        case .hello(let h) where h.version == ipcVersion: return
        case .hello(let h): throw CommandError.versionMismatch(agent: h.version)
        case .refused: throw CommandError.refused
        default: throw CommandError.unexpected
        }
    }

    // MARK: Requests

    /// Sends `request` on the conversation connection and waits for its answer (`refused` is
    /// returned as an answer: what it means depends on the request).
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
                    do { try await c.send(cuerpo) } catch { c.cancel() }
                }
                // The deadline: past it the answers' order is lost, so the connection goes.
                Task { [weak self] in
                    try? await Task.sleep(for: plazo)
                    guard let self, self.pendientes.contains(where: { $0.id == id }) else { return }
                    c.cancel()
                }
            }
        } catch let e as CommandError {
            throw e
        } catch {
            throw .noAnswer
        }
    }
}
