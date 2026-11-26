import Foundation

/// Largest frame body either side sends (`fogar_ports::ipc::MAX_FRAME_BYTES`).
public let maxFrameBytes = 64 * 1024

public enum FrameError: Error, Equatable {
    case tooLarge(Int)
}

/// 4-byte big-endian length + body (ADR-0002). Mirrors `fogar_host::ipc_wire::{frame, take_frame}`.
public func frame(_ body: Data) throws(FrameError) -> Data {
    guard body.count <= maxFrameBytes else { throw .tooLarge(body.count) }
    var out = Data(capacity: 4 + body.count)
    withUnsafeBytes(of: UInt32(body.count).bigEndian) { out.append(contentsOf: $0) }
    out.append(body)
    return out
}

/// Accumulates bytes from a stream and pops complete frames. Partial frames stay buffered.
public struct FrameBuffer: Sendable {
    private var bytes = Data()

    public init() {}

    public mutating func append(_ data: Data) { bytes.append(data) }

    /// The next complete body, `nil` if more bytes are needed. A length over the limit is an error
    /// (the connection must be dropped: the stream can no longer be resynchronised).
    public mutating func next() throws(FrameError) -> Data? {
        guard bytes.count >= 4 else { return nil }
        let start = bytes.startIndex
        let len = bytes[start..<start + 4].reduce(0) { Int($0) << 8 | Int($1) }
        guard len <= maxFrameBytes else { throw .tooLarge(len) }
        guard bytes.count >= 4 + len else { return nil }
        let body = Data(bytes[start + 4..<start + 4 + len])
        // A password may sit in the consumed bytes: overwrite before dropping them.
        bytes.resetBytes(in: start..<start + 4 + len)
        bytes.removeSubrange(start..<start + 4 + len)
        return body
    }
}
