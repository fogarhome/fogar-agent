import FogarWire
import Foundation
import Observation

/// What the app knows about the agent right now. Only what the agent said, or that it is silent.
enum AgentState: Equatable {
    /// First attempt in progress (nothing known yet).
    case looking
    /// No agent listening on the socket.
    case notRunning
    /// The agent speaks another IPC version: one side needs updating.
    case versionMismatch(agent: UInt32)
    /// The agent answered `refused` to `hello` or `subscribe`.
    case refused
    /// Subscribed: the latest status the agent pushed.
    case live(WireStatus)

    var status: WireStatus? {
        if case .live(let s) = self { s } else { nil }
    }
}

/// Outcome of one command on its own short connection (`hello` → request → answer → close).
enum CommandError: Error, Equatable {
    case agentNotRunning
    case versionMismatch(agent: UInt32)
    /// `refused`: on pair/unpair, the agent did not verify this app's signer (uid_only).
    case refused
    case noAnswer
    case unexpected
}

/// The app's single link to the agent: one long subscription for live status, plus a short
/// connection per command (an unsubscribed connection is closed after 30 s idle by the agent, and
/// there are no request ids, so commands never share the event stream). At most 2 of the agent's
/// 4 IPC slots.
@Observable
final class AgentLink {
    private(set) var state: AgentState = .looking
    /// When the current state began (for «desde hace…» and the log in later slices).
    private(set) var since = Date()

    @ObservationIgnored let socketPath: String
    @ObservationIgnored private var loop: Task<Void, Never>?
    @ObservationIgnored private var nap: Task<Void, Never>?

    init(socketPath: String = AgentLink.defaultSocketPath) {
        self.socketPath = socketPath
    }

    static var defaultSocketPath: String {
        #if DEBUG
        AgentSocket.path(allowOverride: true)
        #else
        AgentSocket.defaultPath
        #endif
    }

    // MARK: Subscription

    func start() {
        guard loop == nil else { return }
        loop = Task { await run() }
    }

    func stop() {
        loop?.cancel()
        loop = nil
    }

    /// Cuts the backoff short (window became active, the person pressed «Reintentar»).
    func retryNow() {
        nap?.cancel()
    }

    private func run() async {
        var backoff = Duration.milliseconds(500)
        while !Task.isCancelled {
            do {
                let c = try await FramedConnection.connect(path: socketPath)
                defer { Task { await c.cancel() } }
                try await handshake(c)
                try await c.send(WireCodec.encode(WireRequest.subscribe))
                backoff = .milliseconds(500)
                while !Task.isCancelled {
                    switch try WireCodec.decodeResponse(await c.receive()) {
                    case .event(let s), .status(let s): set(.live(s))
                    case .refused: set(.refused); throw LinkError.closed
                    default: break // `subscribed` and anything else carry nothing to show
                    }
                }
            } catch let CommandError.versionMismatch(agent) {
                set(.versionMismatch(agent: agent))
            } catch CommandError.refused {
                set(.refused)
            } catch {
                // Socket gone, agent restarted, garbage frame: honestly «not running» until it answers.
                set(.notRunning)
            }
            // shortcut: retries on a timer (0.5 s doubling to 8 s); a vnode watch on the run
            // directory would wake exactly when the agent starts. Upgrade if idle cost shows up.
            await sleep(backoff)
            backoff = min(backoff * 2, .seconds(8))
        }
    }

    private func sleep(_ d: Duration) async {
        let t = Task<Void, Never> { try? await Task.sleep(for: d) }
        nap = t
        await withTaskCancellationHandler { await t.value } onCancel: { t.cancel() }
        nap = nil
    }

    private func set(_ new: AgentState) {
        guard new != state else { return }
        if new.phaseKey != state.phaseKey { since = Date() }
        state = new
    }

    // MARK: Commands

    func discover() async throws(CommandError) -> [WireCandidate] {
        switch try await command(.discover, timeout: .seconds(10)) {
        case .discovered(let d): d.gateways
        default: throw .unexpected
        }
    }

    enum PairOutcome: Equatable {
        case paired(WireGateway)
        case failed(WirePairFailure)
    }

    func pair(address: String, code: String) async throws(CommandError) -> PairOutcome {
        // Discovery of the address and the HTTP round trip happen in the agent: allow for both.
        switch try await command(.pair(WirePair(address: address, code: code)), timeout: .seconds(20)) {
        case .paired(let g): .paired(g)
        case .pairFailed(let f): .failed(f.reason)
        default: throw .unexpected
        }
    }

    func unpair() async throws(CommandError) {
        guard case .unpaired = try await command(.unpair, timeout: .seconds(5)) else { throw .unexpected }
    }

    private func command(_ request: WireRequest, timeout: Duration) async throws(CommandError) -> WireResponse {
        let path = socketPath
        do {
            return try await withDeadline(timeout) {
                let c = try await FramedConnection.connect(path: path)
                defer { Task { await c.cancel() } }
                try await self.handshake(c)
                try await c.send(WireCodec.encode(request))
                while true {
                    switch try WireCodec.decodeResponse(await c.receive()) {
                    case .event, .status: continue // not subscribed, but never mistake one for the answer
                    case .refused: throw CommandError.refused
                    case let answer: return answer
                    }
                }
            }
        } catch let e as CommandError {
            throw e
        } catch LinkError.unavailable {
            throw .agentNotRunning
        } catch {
            throw .noAnswer
        }
    }

    /// `hello`: the agent answers with its own version (mismatch included) or `refused`.
    nonisolated private func handshake(_ c: FramedConnection) async throws {
        try await c.send(WireCodec.encode(WireRequest.hello(WireHello(version: ipcVersion))))
        switch try WireCodec.decodeResponse(await c.receive()) {
        case .hello(let h) where h.version == ipcVersion: return
        case .hello(let h): throw CommandError.versionMismatch(agent: h.version)
        case .refused: throw CommandError.refused
        default: throw CommandError.unexpected
        }
    }
}

private extension AgentState {
    /// Changes that reset «since»: a new phase, not a new counter value.
    var phaseKey: String {
        switch self {
        case .looking: "looking"
        case .notRunning: "notRunning"
        case .versionMismatch: "version"
        case .refused: "refused"
        case .live(let s): "live.\(s.link)"
        }
    }
}
