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
    /// The agent answered `refused` to `hello`.
    case refused
    /// The agent answers but refuses `subscribe`: an agent older than S1 (no live status).
    case noLiveStatus
    /// The agent sent something this app cannot decode (e.g. a new phase without a version bump).
    case unintelligible
    /// Something listens on the agent's socket, but it is not provably our agent (signature, user
    /// or directory). Nothing was sent to it.
    case noVerificado
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
    /// `refused`: on login/logout/reconnect, the agent did not verify this app's signer (uid_only);
    /// on reconnect also «nothing to reconnect» (the link left the pause meanwhile).
    case refused
    case noAnswer
    case unexpected
    /// What answers on the socket is not provably our agent: nothing was sent.
    case notOurAgent
    /// `logout` refused by the signed agent because the Keychain item could not be deleted
    /// (`fogar-core::ipc`: «could not delete the account»): the session is still there.
    case sessionNotDeleted
}

/// The app's single link to the agent: one long subscription for live status, plus a short
/// connection per command (an unsubscribed connection is closed after 30 s idle by the agent, and
/// there are no request ids, so commands never share the event stream). The subscription plus at
/// most one connection per command in flight: 3 of the agent's 4 IPC slots when a search and a
/// sign-in overlap.
@Observable
final class AgentLink {
    private(set) var state: AgentState = .looking
    /// When the current state began (for «desde hace…» and the log in later slices).
    private(set) var since = Date()
    /// The agent went from signed in to `not_paired` without this app asking (gateway revocation).
    // shortcut: only seen while the app runs; an app opened after the revocation cannot tell it from
    // «never signed in». Upgrade with the close code in `WireStatus` (contract PR: fogar-ports/testkit).
    private(set) var accesoRetirado = false
    /// Set before this app's own `logout` goes out: its `not_paired` event can beat the answer.
    @ObservationIgnored private var cerrandoSesion = false

    @ObservationIgnored let socketPath: String
    @ObservationIgnored let verificacion: VerificacionPar
    @ObservationIgnored private var loop: Task<Void, Never>?
    @ObservationIgnored private var nap: Task<Void, Never>?

    init(socketPath: String = AgentLink.defaultSocketPath, verificacion: VerificacionPar = AgentLink.verificacionPorDefecto) {
        self.socketPath = socketPath
        self.verificacion = verificacion
    }

    /// What `login`, `logout`, `reconnect` and the conversation require. Release: the signed agent.
    /// Debug builds accept an unsigned server of the same user ONLY when `FOGAR_AGENT_UNSIGNED=1`
    /// is set explicitly (the stand-in, see the README). The read-only status always needs only
    /// the directory and user checks (`verificacionLectura`).
    static var verificacionPorDefecto: VerificacionPar {
        #if DEBUG
        if ProcessInfo.processInfo.environment["FOGAR_AGENT_UNSIGNED"] == "1" { return .rutaYUsuario }
        #endif
        return .agenteFirmado
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
            var permanent = false // retrying soon would not change the answer
            do {
                // Status is read-only: the directory and user checks are enough (an S1 agent not yet
                // signed as `com.fogar.agent` still shows its state).
                let v = VerificacionPar.rutaYUsuario
                let c = try await FramedConnection.connect(path: socketPath, verificacion: v)
                defer { c.cancel() }
                // Every outside await has a deadline; only the event stream itself is open-ended.
                try await withDeadline(.seconds(5)) {
                    try await self.handshake(c, v)
                    try await c.send(WireCodec.encode(WireRequest.subscribe))
                }
                while !Task.isCancelled {
                    switch try WireCodec.decodeResponse(await c.receive()) {
                    case .event(let s), .status(let s):
                        set(.live(s))
                        backoff = .milliseconds(500) // only a real event proves the link
                    case .refused:
                        throw CommandError.unexpected // subscribe refused: see the catch below
                    default:
                        break // `subscribed` carries nothing to show
                    }
                }
            } catch let CommandError.versionMismatch(agent) {
                set(.versionMismatch(agent: agent)); permanent = true
            } catch CommandError.refused {
                set(.refused); permanent = true
            } catch CommandError.unexpected {
                set(.noLiveStatus); permanent = true
            } catch is DecodingError, LinkError.frame {
                set(.unintelligible); permanent = true
            } catch LinkError.notOurAgent {
                set(.noVerificado); permanent = true
            } catch {
                // Socket gone, agent restarted or wedged: honestly «not running» until it answers.
                set(.notRunning)
            }
            // shortcut: retries on a timer (0.5 s doubling to 8 s; 60 s when the agent answered
            // but retrying cannot help); a vnode watch on the run directory would wake exactly
            // when the agent starts. Upgrade if idle cost shows up.
            await sleep(permanent ? .seconds(60) : backoff)
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
        accesoRetirado = Self.retirado(antes: state, despues: new, cerrandoSesion: cerrandoSesion, yaRetirado: accesoRetirado)
        if new.status?.phase == .notPaired { cerrandoSesion = false }
        if new.phaseKey != state.phaseKey { since = Date() }
        state = new
    }

    /// Whether the notice «se retiró el acceso» holds after `antes` → `despues`.
    static func retirado(antes: AgentState, despues: AgentState, cerrandoSesion: Bool, yaRetirado: Bool) -> Bool {
        guard let fase = despues.status?.phase else { return yaRetirado } // agent silent: keep what we knew
        guard fase == .notPaired else { return false }
        // Signed in before = a phase with a gateway (a Keychain error at boot shows `reconnecting`
        // with none, then `not_paired`: never signed in, nothing was withdrawn).
        if let previa = antes.status, previa.phase != .notPaired, previa.gateway != nil { return !cerrandoSesion }
        return yaRetirado
    }

    // MARK: Commands

    func discover() async throws(CommandError) -> [WireCandidate] {
        switch try await command(.discover, timeout: .seconds(10)) {
        case .discovered(let d): d.gateways
        default: throw .unexpected
        }
    }

    enum LoginOutcome: Equatable {
        case loggedIn(WireGateway)
        case failed(WireLoginFailure)
    }

    /// The password goes out once, to the signed agent only, and is not kept here. Never print or
    /// interpolate the request that carries it.
    func login(address: String, username: String, password: String) async throws(CommandError) -> LoginOutcome {
        // The agent's worst case: tailnet lookup (2 s) + discovery probes + `POST /auth/login`
        // under its 10 s `account_timeout` + the Keychain, and after a 201 it cannot keep a
        // best-effort `POST /auth/logout` (another 10 s). Outlast it.
        let pedido = WireRequest.login(WireLogin(address: address, username: username, password: password))
        switch try await command(pedido, timeout: .seconds(35)) {
        case .loggedIn(let g): return .loggedIn(g)
        case .loginFailed(let f): return .failed(f.reason)
        default: throw .unexpected
        }
    }

    /// Signs out. The agent deletes the account always; `true` = the gateway heard the logout.
    func logout() async throws(CommandError) -> Bool {
        cerrandoSesion = true
        do throws(CommandError) {
            // Outlasts the agent's `POST /auth/logout` (10 s), so «el gateway no contestó» can show.
            guard case .loggedOut(let o) = try await command(.logout, timeout: .seconds(15)) else { throw CommandError.unexpected }
            return o.gateway_reached
        } catch {
            // Only when the agent surely did nothing: after `noAnswer` it may still sign out, and its
            // `not_paired` must not read as a revocation (the next `not_paired` clears the flag).
            switch error {
            case .agentNotRunning, .refused, .versionMismatch, .notOurAgent, .sessionNotDeleted: cerrandoSesion = false
            case .noAnswer, .unexpected: break
            }
            throw error
        }
    }

    /// One new attempt from a link parked by the gateway (`parked_missing_scope`,
    /// `parked_agent_refused`, `parked_upgrade_client`); how it goes arrives as live status.
    func reconnect() async throws(CommandError) {
        guard case .reconnecting = try await command(.reconnect, timeout: .seconds(5)) else { throw .unexpected }
    }

    private func command(_ request: WireRequest, timeout: Duration) async throws(CommandError) -> WireResponse {
        let path = socketPath
        // What carries the person's content or changes where it goes needs the signed agent.
        let v: VerificacionPar = switch request {
        case .login, .logout, .reconnect: verificacion
        default: .rutaYUsuario
        }
        do {
            return try await withDeadline(timeout) {
                let c = try await FramedConnection.connect(path: path, verificacion: v)
                defer { c.cancel() }
                try await self.handshake(c, v)
                try await c.send(WireCodec.encode(request))
                while true {
                    switch try WireCodec.decodeResponse(await c.receive()) {
                    case .event, .status: continue // not subscribed, but never mistake one for the answer
                    // The agent's reason for a Keychain delete that failed on `logout` (`fogar-core::ipc`).
                    case .refused(let r) where r.reason == "could not delete the account": throw CommandError.sessionNotDeleted
                    case .refused: throw CommandError.refused
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

    /// `hello`: the agent answers with its own version (mismatch included) or `refused`. The
    /// process that wrote the answer is checked again before anything else is sent.
    nonisolated private func handshake(_ c: FramedConnection, _ v: VerificacionPar) async throws {
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
}

private extension AgentState {
    /// Changes that reset «since»: a new phase, not a new counter value.
    var phaseKey: String {
        switch self {
        case .looking: "looking"
        case .notRunning: "notRunning"
        case .versionMismatch: "version"
        case .refused: "refused"
        case .noLiveStatus: "noLiveStatus"
        case .unintelligible: "unintelligible"
        case .noVerificado: "noVerificado"
        case .live(let s): "live.\(s.link)"
        }
    }
}
