import Foundation

// Hand-written companions to the generated Wire.swift (never edit that file by hand).

/// IPC protocol version this app speaks (`fogar_ports::ipc::IPC_VERSION`). 2 = S1, 3 = reconnect,
/// 4 = conversation (S2), 5 = account (login/logout; pair/unpair removed), 6 = the conversations
/// over `/events` (list, open, new; messages and turns as entries; no `turn_visible`).
public let ipcVersion: UInt32 = 6

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

/// `print` and interpolation of a login (or a `WireRequest` holding one) never show the password.
/// `dump` and `Mirror` still do: never use them on a request.
extension WireLogin: CustomStringConvertible, CustomDebugStringConvertible {
    public var description: String { "WireLogin(address: \(address), username: \(username), password: <redacted>)" }
    public var debugDescription: String { description }
}

// MARK: Tolerant reading of the conversation (an agent newer than this app)

extension WireCodec {
    /// Like `decodeResponse`, but a value this app does not know inside a conversation frame never
    /// costs the connection: an unknown enum value in the view reads as its closest known meaning
    /// (an unknown activity as `awaiting_reply`, an unknown outcome as `error`, an unknown progress
    /// as `working`, an unknown source as `push`, an unknown message state as `delivered`), an
    /// entry of an unknown kind is left out, and an undecodable answer to a request reads as
    /// `refused`, so the answers' order is kept. A frame of an unknown type still throws.
    public static func decodeResponseTolerante(_ body: Data) throws -> WireResponse {
        do {
            return try decodeResponse(body)
        } catch let e as DecodingError {
            struct Tipo: Decodable { let type: String }
            guard let t = try? JSONDecoder().decode(Tipo.self, from: body) else { throw e }
            switch t.type {
            case "conversation":
                struct Sobre: Decodable { let data: ConversacionTolerante }
                return .conversation(try JSONDecoder().decode(Sobre.self, from: body).data.wire)
            case "capture_refused", "text_refused", "mic_permission":
                return .refused(WireRefused(reason: "unintelligible answer"))
            default:
                throw e
            }
        }
    }
}

struct TurnoTolerante: Decodable {
    let wire: WireTurn

    enum K: String, CodingKey {
        case turn_id, source, by_name, by_self, reply, progress, tool, outcome, error_code
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        let progreso = try c.decodeIfPresent(String.self, forKey: .progress)
        let desenlace = try c.decodeIfPresent(String.self, forKey: .outcome)
        let fuente = try c.decode(String.self, forKey: .source)
        wire = WireTurn(
            turn_id: try c.decode(String.self, forKey: .turn_id),
            source: WireSource(rawValue: fuente) ?? WireSource.push,
            by_name: try c.decodeIfPresent(String.self, forKey: .by_name),
            by_self: try c.decode(Bool.self, forKey: .by_self),
            reply: try c.decode(String.self, forKey: .reply),
            progress: progreso.map { WireProgress(rawValue: $0) ?? WireProgress.working },
            tool: try c.decodeIfPresent(String.self, forKey: .tool),
            outcome: desenlace.map { WireOutcome(rawValue: $0) ?? WireOutcome.error },
            error_code: try c.decodeIfPresent(String.self, forKey: .error_code))
    }
}

struct MensajeTolerante: Decodable {
    let wire: WireMessage

    enum K: String, CodingKey {
        case id, text, mine, from_name, state, error_code
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        let estado = try c.decode(String.self, forKey: .state)
        wire = WireMessage(
            id: try c.decode(String.self, forKey: .id),
            text: try c.decode(String.self, forKey: .text),
            mine: try c.decode(Bool.self, forKey: .mine),
            from_name: try c.decodeIfPresent(String.self, forKey: .from_name),
            state: WireMessageState(rawValue: estado) ?? WireMessageState.delivered,
            error_code: try c.decodeIfPresent(String.self, forKey: .error_code))
    }
}

/// One entry; `wire` is nil for a kind this app does not know (left out of the view).
struct EntradaTolerante: Decodable {
    let wire: WireEntry?

    enum K: String, CodingKey { case type, data }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        switch try c.decode(String.self, forKey: .type) {
        case "message": wire = .message(try c.decode(MensajeTolerante.self, forKey: .data).wire)
        case "turn": wire = .turn(try c.decode(TurnoTolerante.self, forKey: .data).wire)
        default: wire = nil
        }
    }
}

struct ConversacionTolerante: Decodable {
    let wire: WireConversation

    enum K: String, CodingKey {
        case id, activity, live, draft, entries, mic, transcriber, notice, read_only
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        let actividad = try c.decode(String.self, forKey: .activity)
        let mic = try c.decode(String.self, forKey: .mic)
        let transcriptor = try c.decode(String.self, forKey: .transcriber)
        let aviso = try c.decodeIfPresent(String.self, forKey: .notice)
        wire = WireConversation(
            id: try c.decodeIfPresent(String.self, forKey: .id),
            // An unknown activity is «Fogar is busy» (neutral), never `idle`: an `idle` guess could
            // hide a take that is still open.
            activity: WireActivity(rawValue: actividad) ?? WireActivity.awaitingReply,
            live: try c.decode(String.self, forKey: .live),
            draft: try c.decodeIfPresent(String.self, forKey: .draft),
            entries: try c.decode([EntradaTolerante].self, forKey: .entries).compactMap(\.wire),
            mic: WireMic(rawValue: mic) ?? WireMic.unknown,
            transcriber: WireService(rawValue: transcriptor) ?? WireService.unknown,
            notice: aviso.flatMap { WireNotice(rawValue: $0) },
            read_only: try c.decode(Bool.self, forKey: .read_only))
    }
}
