import Darwin
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
    /// Something answers on the socket, but it is not provably our agent (wrong owner or mode of
    /// the run directory, another user, or not signed like this app with the agent's identifier).
    /// Nothing was sent to it.
    case notOurAgent
}

/// `sockaddr_un.sun_path` holds 104 bytes including the NUL: a longer path cannot be connected
/// to, and Network.framework traps on it instead of failing.
public let maxSocketPathBytes = 103

/// One framed stream over a Unix socket. The app's side is a plain socket it can authenticate
/// (`connect`: the server is checked before a byte is sent); the stand-in's accepted side is a
/// Network.framework connection. Every call that waits is cancellable: cancelling the task
/// cancels the connection.
public actor FramedConnection {
    private enum Transporte {
        case red(NWConnection)
        case fd(Enchufe)
    }

    private let transporte: Transporte
    private var buffer = FrameBuffer()
    private let queue = DispatchQueue(label: "fogar.ipc.connection")

    public init(accepted connection: NWConnection) {
        transporte = .red(connection)
    }

    private init(enchufe: Enchufe) {
        transporte = .fd(enchufe)
    }

    /// Connects to `path` and authenticates the server (`verificacion`) before anything is sent.
    /// Fails fast with `.unavailable` when nothing listens there and `.notOurAgent` when what
    /// listens is not provably our agent.
    public static func connect(path: String, verificacion: VerificacionPar = .agenteFirmado,
                               timeout: Duration = .seconds(2)) async throws -> FramedConnection {
        guard path.utf8.count <= maxSocketPathBytes else { throw LinkError.unavailable }
        let e = try await withDeadline(timeout) { try await Enchufe.conectar(path: path, verificacion: verificacion) }
        return FramedConnection(enchufe: e)
    }

    /// The agent behind this connection still passes the check (it wrote last: call after
    /// reading its answer). Throws `.notOurAgent` and closes the connection otherwise.
    public func reverificar(_ verificacion: VerificacionPar) throws {
        guard case .fd(let e) = transporte else { return }
        guard PeerCheck.servidorValido(fd: e.fd, verificacion: verificacion) else {
            e.cerrar()
            throw LinkError.notOurAgent
        }
    }

    /// Starts an accepted connection and waits until it is ready (stand-in side only).
    public func start() async throws {
        guard case .red(let connection) = transporte else { return }
        let once = Once()
        let queue = self.queue
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (k: CheckedContinuation<Void, Error>) in
                connection.stateUpdateHandler = { state in
                    switch state {
                    case .ready: once.run { k.resume() }
                    case .waiting, .failed:
                        once.run { k.resume(throwing: LinkError.unavailable) }
                        connection.cancel()
                    case .cancelled: once.run { k.resume(throwing: LinkError.closed) }
                    default: break
                    }
                }
                // Cancelled before the handler above existed: `onCancel` already ran, and a
                // cancelled NWConnection may never report `.cancelled` to a late handler.
                if Task.isCancelled {
                    once.run { k.resume(throwing: LinkError.closed) }
                    return
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
        switch transporte {
        case .fd(let e):
            try await e.escribir(bytes)
        case .red(let connection):
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
    }

    /// The next complete frame body.
    public func receive() async throws -> Data {
        while true {
            do {
                if let body = try buffer.next() { return body }
            } catch {
                cancel()
                throw LinkError.frame(error)
            }
            buffer.append(try await readChunk())
        }
    }

    /// Thread-safe, so callers can tear down synchronously.
    public nonisolated func cancel() {
        switch transporte {
        case .red(let connection): connection.cancel()
        case .fd(let e): e.cerrar()
        }
    }

    private func readChunk() async throws -> Data {
        switch transporte {
        case .fd(let e):
            return try await e.leer(maximo: 4 + maxFrameBytes)
        case .red(let connection):
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
}

/// The app's end of the socket: a plain file descriptor (Network.framework hides it, and the
/// server can only be authenticated on the descriptor itself). Reads and writes block on their
/// own queue, never on the caller's; `cerrar` shuts the socket down, which wakes a blocked read.
/// The descriptor is closed when the last reference goes (no reuse while a read may use it).
final class Enchufe: @unchecked Sendable {
    let fd: Int32
    private let lectura = DispatchQueue(label: "fogar.ipc.read")
    private let escritura = DispatchQueue(label: "fogar.ipc.write")

    private init(fd: Int32) { self.fd = fd }

    deinit { Darwin.close(fd) }

    static func conectar(path: String, verificacion: VerificacionPar) async throws -> Enchufe {
        // Before connecting: the socket and its directory are ours (no symlink, no other owner).
        guard PeerCheck.rutaPropia(path) else {
            var st = stat()
            throw lstat(path, &st) == 0 ? LinkError.notOurAgent : LinkError.unavailable
        }
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw LinkError.unavailable }
        let e = Enchufe(fd: fd)
        var si: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &si, socklen_t(MemoryLayout<Int32>.size))
        // Writes never hang: a stuck agent is a closed connection after 5 s.
        var plazo = timeval(tv_sec: 5, tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &plazo, socklen_t(MemoryLayout<timeval>.size))
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        let bytes = Array(path.utf8)
        withUnsafeMutableBytes(of: &addr.sun_path) { dst in
            for (i, b) in bytes.enumerated() where i < dst.count - 1 { dst[i] = b }
        }
        // Non-blocking connect: a Unix socket connects at once or fails (a full backlog = EAGAIN).
        let flags = fcntl(fd, F_GETFL)
        _ = fcntl(fd, F_SETFL, flags | O_NONBLOCK)
        let r = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard r == 0 else { throw LinkError.unavailable }
        _ = fcntl(fd, F_SETFL, flags & ~O_NONBLOCK)
        // The listener is the process behind the peer socket now (nothing was written yet).
        guard PeerCheck.servidorValido(fd: fd, verificacion: verificacion) else { throw LinkError.notOurAgent }
        return e
    }

    func cerrar() { _ = Darwin.shutdown(fd, SHUT_RDWR) }

    func leer(maximo: Int) async throws -> Data {
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (k: CheckedContinuation<Data, Error>) in
                lectura.async { [self] in
                    var buf = [UInt8](repeating: 0, count: maximo)
                    while true {
                        let n = buf.withUnsafeMutableBytes { Darwin.read(fd, $0.baseAddress, maximo) }
                        if n > 0 { k.resume(returning: Data(buf[0..<n])); return }
                        if n < 0, errno == EINTR { continue }
                        k.resume(throwing: LinkError.closed)
                        return
                    }
                }
            }
        } onCancel: {
            cerrar()
        }
    }

    func escribir(_ data: Data) async throws {
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (k: CheckedContinuation<Void, Error>) in
                escritura.async { [self] in
                    let ok = data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) -> Bool in
                        var hecho = 0
                        while hecho < raw.count {
                            let n = Darwin.write(fd, raw.baseAddress! + hecho, raw.count - hecho)
                            if n > 0 { hecho += n; continue }
                            if n < 0, errno == EINTR { continue }
                            return false
                        }
                        return true
                    }
                    if ok { k.resume() } else { cerrar(); k.resume(throwing: LinkError.closed) }
                }
            }
        } onCancel: {
            cerrar()
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
