import Foundation

// Hand-written companions to the generated Wire.swift (never edit that file by hand).

/// IPC protocol version this app speaks (`fogar_ports::ipc::IPC_VERSION`). 2 = S1, 3 = reconnect,
/// 4 = conversation (S2), 5 = account (login/logout; the app signs in from S4 PR 3 Task 3).
public let ipcVersion: UInt32 = 5

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

// MARK: Tolerant reading of the conversation (an agent newer than this app)

extension WireCodec {
    /// Like `decodeResponse`, but a value this app does not know inside a conversation frame never
    /// costs the connection: an unknown enum value in the view reads as its closest known meaning
    /// (an unknown activity as `awaiting_reply`, an unknown outcome as `error`, an unknown progress
    /// as `working`, an unknown source as
    /// `push`, which only sounds when on screen), and an undecodable answer to a request reads as
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
        case turn_id, conversation, source, mine, reply, progress, tool, outcome, error_code, audio
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        let progreso = try c.decodeIfPresent(String.self, forKey: .progress)
        let desenlace = try c.decodeIfPresent(String.self, forKey: .outcome)
        let fuente = try c.decode(String.self, forKey: .source)
        let audio = try c.decode(String.self, forKey: .audio)
        wire = WireTurn(
            turn_id: try c.decode(String.self, forKey: .turn_id),
            conversation: try c.decodeIfPresent(String.self, forKey: .conversation),
            source: WireSource(rawValue: fuente) ?? WireSource.push,
            mine: try c.decode(String.self, forKey: .mine),
            reply: try c.decode(String.self, forKey: .reply),
            progress: progreso.map { WireProgress(rawValue: $0) ?? WireProgress.working },
            tool: try c.decodeIfPresent(String.self, forKey: .tool),
            outcome: desenlace.map { WireOutcome(rawValue: $0) ?? WireOutcome.error },
            error_code: try c.decodeIfPresent(String.self, forKey: .error_code),
            audio: WireAudio(rawValue: audio) ?? WireAudio.none)
    }
}

struct ConversacionTolerante: Decodable {
    let wire: WireConversation

    enum K: String, CodingKey {
        case activity, live, draft, turns, speaking, awaiting_visible, mic, transcriber, tts, notice
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        let actividad = try c.decode(String.self, forKey: .activity)
        let mic = try c.decode(String.self, forKey: .mic)
        let transcriptor = try c.decode(String.self, forKey: .transcriber)
        let tts = try c.decode(String.self, forKey: .tts)
        let aviso = try c.decodeIfPresent(String.self, forKey: .notice)
        wire = WireConversation(
            // An unknown activity is «Fogar is busy» (neutral), never `idle`: an `idle` guess could
            // hide a take that is still open.
            activity: WireActivity(rawValue: actividad) ?? WireActivity.awaitingReply,
            live: try c.decode(String.self, forKey: .live),
            draft: try c.decodeIfPresent(String.self, forKey: .draft),
            turns: try c.decode([TurnoTolerante].self, forKey: .turns).map(\.wire),
            speaking: try c.decode(Bool.self, forKey: .speaking),
            awaiting_visible: try c.decodeIfPresent(String.self, forKey: .awaiting_visible),
            mic: WireMic(rawValue: mic) ?? WireMic.unknown,
            transcriber: WireService(rawValue: transcriptor) ?? WireService.unknown,
            tts: WireService(rawValue: tts) ?? WireService.unknown,
            notice: aviso.flatMap { WireNotice(rawValue: $0) })
    }
}
