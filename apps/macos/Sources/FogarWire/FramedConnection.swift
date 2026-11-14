import Foundation
import Network

/// Where the agent listens (ADR-0002). `FOGAR_AGENT_SOCKET` overrides it for development (the app
/// honours it only in debug builds; see `AgentLink.socketPath`).
public enum AgentSocket {
    public static var defaultPath: String {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/com.fogar.agent/run/agent.sock").path
    }

    public static func path(allowOverride: Bool) -> String {
        if allowOverride, let p = ProcessInfo.processInfo.environment["FOGAR_AGENT_SOCKET"], !p.isEmpty {
            return p
        }
        return defaultPath
    }
}

public enum LinkError: Error, Equatable {
    /// No socket, or nobody accepting on it: the agent is not running.
    case unavailable
    /// The other side closed the connection.
    case closed
    case timedOut
    case frame(FrameError)
    case malformed
}

/// One framed stream over a Unix socket (Network.framework). Used by the app (client) and by the
/// stand-in agent (server side of an accepted connection). Every call that waits is cancellable:
/// cancelling the task cancels the connection.
public actor FramedConnection {
    private let connection: NWConnection
    private var buffer = FrameBuffer()
    private let queue = DispatchQueue(label: "fogar.ipc.connection")

    public init(accepted connection: NWConnection) {
        self.connection = connection
    }

    /// Connects to `path`. Fails fast with `.unavailable` when nothing listens there.
    public static func connect(path: String, timeout: Duration = .seconds(2)) async throws -> FramedConnection {
        let c = FramedConnection(accepted: NWConnection(to: .unix(path: path), using: .tcp))
        try await withDeadline(timeout) { try await c.start() }
        return c
    }

    /// Starts an accepted or outgoing connection and waits until it is ready.
    public func start() async throws {
        let connection = self.connection
        let once = Once()
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (k: CheckedContinuation<Void, Error>) in
                connection.stateUpdateHandler = { state in
                    switch state {
                    case .ready: once.run { k.resume() }
                    // `.waiting` on a Unix socket means ENOENT/ECONNREFUSED: nobody is there.
                    case .waiting, .failed:
                        once.run { k.resume(throwing: LinkError.unavailable) }
                        connection.cancel()
                    case .cancelled: once.run { k.resume(throwing: LinkError.closed) }
                    default: break
                    }
                }
                connection.start(queue: queue)
            }
        } onCancel: {
            connection.cancel()
        }
    }

    public func send(_ body: Data) async throws {
        let bytes: Data
        do { bytes = try frame(body) } catch { throw LinkError.frame(error) }
        let connection = self.connection
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (k: CheckedContinuation<Void, Error>) in
                connection.send(content: bytes, completion: .contentProcessed { error in
                    if error != nil { k.resume(throwing: LinkError.closed) } else { k.resume() }
                })
            }
        } onCancel: {
            connection.cancel()
        }
    }

    /// The next complete frame body.
    public func receive() async throws -> Data {
        while true {
            do {
                if let body = try buffer.next() { return body }
            } catch {
                connection.cancel()
                throw LinkError.frame(error)
            }
            buffer.append(try await readChunk())
        }
    }

    public func cancel() { connection.cancel() }

    private func readChunk() async throws -> Data {
        let connection = self.connection
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (k: CheckedContinuation<Data, Error>) in
                connection.receive(minimumIncompleteLength: 1, maximumLength: 4 + maxFrameBytes) { data, _, done, error in
                    if let data, !data.isEmpty {
                        k.resume(returning: data)
                    } else if error != nil || done {
                        k.resume(throwing: LinkError.closed)
                    } else {
                        k.resume(returning: Data())
                    }
                }
            }
        } onCancel: {
            connection.cancel()
        }
    }
}

/// Runs `body` once, whichever callback gets there first (the state handler can fire repeatedly).
private final class Once: @unchecked Sendable {
    private let lock = NSLock()
    private var done = false
    func run(_ body: () -> Void) {
        lock.lock()
        defer { lock.unlock() }
        guard !done else { return }
        done = true
        body()
    }
}

/// Races `op` against a deadline; the loser is cancelled.
public func withDeadline<T: Sendable>(_ limit: Duration, _ op: @escaping @Sendable () async throws -> T) async throws -> T {
    try await withThrowingTaskGroup(of: T.self) { group in
        group.addTask { try await op() }
        group.addTask {
            try await Task.sleep(for: limit)
            throw LinkError.timedOut
        }
        defer { group.cancelAll() }
        guard let first = try await group.next() else { throw LinkError.timedOut }
        return first
    }
}
