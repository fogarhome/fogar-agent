import Foundation
import Network
import Testing
@testable import FogarWire

/// The golden JSON pinned in `crates/fogar-host/src/ipc_wire.rs` (`golden_json`): what the Swift
/// app must parse and produce. If Rust changes it, IPC_VERSION bumps and these fail.
@Suite struct GoldenJSON {
    @Test func eventDecodes() throws {
        let json = #"{"type":"event","data":{"link":"ready","phase":"ready","gateway":{"address":"http://green-house:8004","name":"green-house","version":"1.29.0","client_name":"Mac de Sito","user":"Sito"},"sessions":3,"turns_completed":7,"boot_mode":"normal","peer_check":"same_signer","transcriber":"up","tts":"down","read_only":false}}"#
        guard case .event(let s) = try WireCodec.decodeResponse(Data(json.utf8)) else {
            Issue.record("not an event"); return
        }
        #expect(!s.read_only)
        #expect(s.phase == .ready)
        #expect(s.gateway?.name == "green-house")
        #expect(s.gateway?.client_name == "Mac de Sito")
        #expect(s.gateway?.user == "Sito")
        #expect(s.sessions == 3 && s.turns_completed == 7)
        #expect(s.peer_check == .sameSigner)
    }

    /// IPC v5: the account answers.
    @Test func accountAnswersDecode() throws {
        guard case .loggedIn(let g) = try WireCodec.decodeResponse(
            Data(#"{"type":"logged_in","data":{"address":"http://green-house:8004","name":"green-house","version":"1.29.0","client_name":"Mac de Sito","user":"Sito"}}"#.utf8)) else {
            Issue.record("not logged_in"); return
        }
        #expect(g.name == "green-house" && g.user == "Sito")
        guard case .loginFailed(let f) = try WireCodec.decodeResponse(
            Data(#"{"type":"login_failed","data":{"reason":"needs_secure_address"}}"#.utf8)) else {
            Issue.record("not login_failed"); return
        }
        #expect(f.reason == .needsSecureAddress)
        guard case .loggedOut(let o) = try WireCodec.decodeResponse(
            Data(#"{"type":"logged_out","data":{"gateway_reached":false}}"#.utf8)) else {
            Issue.record("not logged_out"); return
        }
        #expect(!o.gateway_reached)
    }

    /// Pairing left the vocabulary in v5: an agent still sending it is not understood.
    @Test func pairingIsGone() {
        for json in [#"{"type":"pair_failed","data":{"reason":"invalid_code"}}"#, #"{"type":"unpaired"}"#] {
            #expect(throws: (any Error).self) { try WireCodec.decodeResponse(Data(json.utf8)) }
        }
    }

    @Test func datalessDecodes() throws {
        guard case .subscribed = try WireCodec.decodeResponse(Data(#"{"type":"subscribed"}"#.utf8)) else {
            Issue.record("not subscribed"); return
        }
    }

    @Test func notPairedStatusHasNullGateway() throws {
        let json = #"{"type":"status","data":{"link":"not_paired","phase":"not_paired","gateway":null,"sessions":0,"turns_completed":0,"boot_mode":"safe","peer_check":"uid_only","transcriber":"unknown","tts":"unknown","read_only":true}}"#
        guard case .status(let s) = try WireCodec.decodeResponse(Data(json.utf8)) else {
            Issue.record("not a status"); return
        }
        #expect(s.gateway == nil && s.peer_check == .uidOnly && s.boot_mode == "safe")
    }

    /// IPC v6: the conversation view (messages and turns as entries) and the answers.
    @Test func conversationDecodes() throws {
        let json = #"{"type":"conversation","data":{"id":"0b9d","activity":"recording","live":"pon la","draft":"borrador","entries":[{"type":"message","data":{"id":"r-1","text":"¿qué tiempo hace?","mine":false,"from_name":"Cocina","state":"pending","error_code":null}},{"type":"turn","data":{"turn_id":"t-3","source":"mine","by_name":"Cocina","by_self":false,"reply":"Soleado.","progress":"tool","tool":"ha_call_service","outcome":null,"error_code":null}},{"type":"turn","data":{"turn_id":"t-4","source":"push","by_name":null,"by_self":false,"reply":"Recuerda la cita.","progress":null,"tool":null,"outcome":"error","error_code":"TURN_ERROR"}},{"type":"message","data":{"id":"","text":"otra","mine":true,"from_name":null,"state":"failed","error_code":"missing_scope"}}],"mic":"granted","transcriber":"degraded","notice":"silence_cut","read_only":true}}"#
        guard case .conversation(let c) = try WireCodec.decodeResponse(Data(json.utf8)) else {
            Issue.record("not a conversation"); return
        }
        #expect(c.id == "0b9d" && c.activity == .recording && c.live == "pon la" && c.draft == "borrador")
        #expect(c.entries.count == 4 && c.read_only && c.mic == .granted && c.notice == .silenceCut)
        guard case .message(let m) = c.entries[0], case .turn(let t) = c.entries[1], case .turn(let p) = c.entries[2],
              case .message(let f) = c.entries[3] else {
            Issue.record("entries"); return
        }
        #expect(m.state == .pending && m.from_name == "Cocina" && !m.mine)
        #expect(t.tool == "ha_call_service" && t.progress == .tool && t.by_name == "Cocina")
        #expect(p.source == .push && p.outcome == .error)
        #expect(f.state == .failed && f.error_code == "missing_scope" && f.mine)
        guard case .conversations(let l) = try WireCodec.decodeResponse(Data(#"{"type":"conversations","data":{"conversations":[{"id":"0b9d","title":"La serie","private":false,"last_turn_at":"2026-11-27T10:00:00Z","running":true},{"id":"c-2","title":null,"private":true,"last_turn_at":null,"running":false}]}}"#.utf8)) else {
            Issue.record("not conversations"); return
        }
        #expect(l.conversations.count == 2 && l.conversations[0].running && l.conversations[1].private)
        for (json, ok) in [(#"{"type":"list_subscribed"}"#, WireResponse.listSubscribed),
                           (#"{"type":"conversation_opened"}"#, .conversationOpened),
                           (#"{"type":"text_refused","data":{"reason":"missing_scope"}}"#,
                            .textRefused(WireTextRefused(reason: .missingScope)))] {
            #expect(try WireCodec.decodeResponse(Data(json.utf8)) == ok)
        }
        // v6: the turn confirmation left the vocabulary.
        #expect(throws: (any Error).self) { try WireCodec.decodeResponse(Data(#"{"type":"turn_visible_noted"}"#.utf8)) }
        guard case .captureRefused(let r) = try WireCodec.decodeResponse(
            Data(#"{"type":"capture_refused","data":{"reason":"permission_not_determined"}}"#.utf8)) else {
            Issue.record("not capture_refused"); return
        }
        #expect(r.reason == .permissionNotDetermined)
        guard case .micPermission(let m) = try WireCodec.decodeResponse(
            Data(#"{"type":"mic_permission","data":{"state":"not_determined"}}"#.utf8)) else {
            Issue.record("not mic_permission"); return
        }
        #expect(m.state == .notDetermined)
    }

    /// Requests are compared as JSON objects: JSONEncoder's key order is not stable.
    @Test(arguments: [
        // Fixed test credentials only: a failing expectation prints both sides.
        (WireRequest.login(WireLogin(address: "https://casa.ts.net", username: "sito", password: "p\"w")),
         #"{"type":"login","data":{"address":"https://casa.ts.net","username":"sito","password":"p\"w"}}"#),
        (.logout, #"{"type":"logout"}"#),
        (.hello(WireHello(version: 2)), #"{"type":"hello","data":{"version":2}}"#),
        (.subscribe, #"{"type":"subscribe"}"#),
        (.discover, #"{"type":"discover"}"#),
        (.subscribeConversation, #"{"type":"subscribe_conversation"}"#),
        (.sendText(WireSendText(text: "hola \"Fogar\"\n")), #"{"type":"send_text","data":{"text":"hola \"Fogar\"\n"}}"#),
        (.captureStart(WireCaptureStart(gesture: .hold)), #"{"type":"capture_start","data":{"gesture":"hold"}}"#),
        (.captureEnd(WireCaptureEnd(then: .review)), #"{"type":"capture_end","data":{"then":"review"}}"#),
        (.cancelTurn, #"{"type":"cancel_turn"}"#),
        (.requestMicPermission, #"{"type":"request_mic_permission"}"#),
        (.listConversations, #"{"type":"list_conversations"}"#),
        (.openConversation(WireOpen(id: "0b9d")), #"{"type":"open_conversation","data":{"id":"0b9d"}}"#),
        (.newConversation, #"{"type":"new_conversation"}"#),
    ])
    func requestsMatchRust(request: WireRequest, golden: String) throws {
        let ours = try JSONSerialization.jsonObject(with: WireCodec.encode(request)) as? NSDictionary
        let theirs = try JSONSerialization.jsonObject(with: Data(golden.utf8)) as? NSDictionary
        #expect(ours == theirs)
    }

    @Test func loginNeverPrintsThePassword() {
        let req = WireRequest.login(WireLogin(address: "https://casa.ts.net", username: "sito", password: "hunter2"))
        for s in [String(describing: req), String(reflecting: req), "\(req)"] {
            #expect(!s.contains("hunter2"), "\(s)")
            #expect(s.contains("<redacted>"), "\(s)")
        }
    }

    @Test func unknownResponseTypeIsAnError() {
        #expect(throws: (any Error).self) {
            try WireCodec.decodeResponse(Data(#"{"type":"exec","data":{}}"#.utf8))
        }
    }
}

@Suite struct Framing {
    @Test func roundTripAndPartialFrames() throws {
        let f = try frame(Data("{}".utf8))
        #expect(Array(f) == [0, 0, 0, 2, UInt8(ascii: "{"), UInt8(ascii: "}")])
        var b = FrameBuffer()
        b.append(f.prefix(3))
        #expect(try b.next() == nil)
        b.append(f.dropFirst(3) + f)
        #expect(try b.next() == Data("{}".utf8))
        #expect(try b.next() == Data("{}".utf8))
        #expect(try b.next() == nil)
    }

    @Test func limits() throws {
        #expect(throws: FrameError.tooLarge(maxFrameBytes + 1)) { try frame(Data(count: maxFrameBytes + 1)) }
        var b = FrameBuffer()
        b.append(Data([0, 1, 0, 1])) // 65537
        #expect(throws: FrameError.tooLarge(maxFrameBytes + 1)) { try b.next() }
        #expect(try frame(Data(count: maxFrameBytes)).count == maxFrameBytes + 4)
    }
}

/// End to end over a real Unix socket against an in-test listener: framing + connection.
@Suite struct OverASocket {
    /// sun_path holds 104 bytes: a longer path must fail cleanly (Network.framework would trap).
    @Test func overlongSocketPathIsUnavailableNotACrash() async {
        let path = "/tmp/" + String(repeating: "a", count: 120) + ".sock"
        await #expect(throws: LinkError.unavailable) {
            _ = try await FramedConnection.connect(path: path)
        }
    }

    @Test func connectToNothingIsUnavailable() async {
        let path = NSTemporaryDirectory() + "fogar-none-\(UUID().uuidString.prefix(8)).sock"
        await #expect(throws: LinkError.unavailable) {
            _ = try await FramedConnection.connect(path: path)
        }
    }
}

/// A private directory (0700) for a test socket, removed by the caller.
private func directorioPrivado(_ modo: mode_t = 0o700) throws -> String {
    let dir = NSTemporaryDirectory() + "fogar-w-\(UUID().uuidString.prefix(8))"
    try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: false)
    _ = chmod(dir, modo)
    return dir
}

/// Binds a Unix socket at `path` (no listen: enough for the path checks).
private func socketEn(_ path: String) -> Int32 {
    let fd = socket(AF_UNIX, SOCK_STREAM, 0)
    var addr = sockaddr_un()
    addr.sun_family = sa_family_t(AF_UNIX)
    let bytes = Array(path.utf8)
    withUnsafeMutableBytes(of: &addr.sun_path) { dst in
        for (i, b) in bytes.enumerated() where i < dst.count - 1 { dst[i] = b }
    }
    _ = withUnsafePointer(to: &addr) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
    }
    return fd
}

/// The app only talks to a socket of ours in a private directory of ours (ADR-0002 mirrored).
@Suite struct RutaDelSocket {
    @Test func socketPropioEnDirectorioPrivado() throws {
        let dir = try directorioPrivado()
        defer { try? FileManager.default.removeItem(atPath: dir) }
        let fd = socketEn(dir + "/a.sock")
        defer { _ = close(fd) }
        #expect(PeerCheck.rutaPropia(dir + "/a.sock"))
    }

    @Test func directorioAbiertoNo() throws {
        let dir = try directorioPrivado(0o755)
        defer { try? FileManager.default.removeItem(atPath: dir) }
        let fd = socketEn(dir + "/a.sock")
        defer { _ = close(fd) }
        #expect(!PeerCheck.rutaPropia(dir + "/a.sock"))
    }

    @Test func directorioEnlazadoNo() throws {
        let dir = try directorioPrivado()
        let enlace = dir + "-enlace"
        defer { try? FileManager.default.removeItem(atPath: dir); _ = unlink(enlace) }
        let fd = socketEn(dir + "/a.sock")
        defer { _ = close(fd) }
        #expect(symlink(dir, enlace) == 0)
        #expect(!PeerCheck.rutaPropia(enlace + "/a.sock"))
    }

    @Test func unFicheroQueNoEsSocketNo() throws {
        let dir = try directorioPrivado()
        defer { try? FileManager.default.removeItem(atPath: dir) }
        #expect(FileManager.default.createFile(atPath: dir + "/a.sock", contents: Data("x".utf8)))
        #expect(!PeerCheck.rutaPropia(dir + "/a.sock"))
        // A socket of another user cannot be made in a test without root: follow-up.
    }
}

/// The app's own socket transport (`Enchufe`): frames split across reads and joined in one.
@Suite(.serialized) struct TramasPorElEnchufe {
    @Test func trozosYJuntas() async throws {
        let dir = try directorioPrivado()
        defer { try? FileManager.default.removeItem(atPath: dir) }
        let path = dir + "/a.sock"
        let uno = try frame(Data(#"{"type":"subscribed"}"#.utf8))
        let dos = try frame(Data(#"{"type":"reconnecting"}"#.utf8))
        let params = NWParameters.tcp
        params.requiredLocalEndpoint = .unix(path: path)
        let l = try NWListener(using: params)
        defer { l.cancel() }
        l.newConnectionHandler = { conn in
            conn.start(queue: .global())
            // The first frame in two writes, 100 ms apart; then two frames in one write.
            conn.send(content: uno.prefix(3), completion: .contentProcessed { _ in
                DispatchQueue.global().asyncAfter(deadline: .now() + 0.1) {
                    conn.send(content: uno.dropFirst(3) + dos + dos, completion: .contentProcessed { _ in })
                }
            })
        }
        l.start(queue: .global())
        try await Task.sleep(for: .milliseconds(200))
        let c = try await FramedConnection.connect(path: path, verificacion: .rutaYUsuario)
        defer { c.cancel() }
        #expect(try WireCodec.decodeResponse(await c.receive()) == .subscribed)
        #expect(try WireCodec.decodeResponse(await c.receive()) == .reconnecting)
        #expect(try WireCodec.decodeResponse(await c.receive()) == .reconnecting)
    }
}
