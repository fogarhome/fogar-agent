import Foundation

// Hand-written companions to the generated Wire.swift (never edit that file by hand).

/// IPC protocol version this app speaks (`fogar_ports::ipc::IPC_VERSION`). 2 = S1, 3 = reconnect.
public let ipcVersion: UInt32 = 3

/// The pairing code as the agent accepts it (`PairCode::parse`): whitespace (any kind) and `-`
/// removed, then exactly 6 ASCII digits. `nil` means "do not send": a typo must never spend one of
/// the gateway's 5 attempts.
public func parsePairCode(_ typed: String) -> String? {
    let digits = typed.unicodeScalars.filter {
        !CharacterSet.whitespacesAndNewlines.contains($0) && $0 != "-"
    }
    guard digits.count == 6, digits.allSatisfy({ ("0"..."9").contains($0) }) else { return nil }
    return String(String.UnicodeScalarView(digits))
}

public enum WireCodec {
    public static func encode(_ value: some Encodable) throws -> Data {
        try JSONEncoder().encode(value)
    }

    public static func decodeResponse(_ body: Data) throws -> WireResponse {
        try JSONDecoder().decode(WireResponse.self, from: body)
    }

    public static func decodeRequest(_ body: Data) throws -> WireRequest {
        try JSONDecoder().decode(WireRequest.self, from: body)
    }
}
