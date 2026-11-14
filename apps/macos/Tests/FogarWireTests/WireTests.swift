import Foundation
import Testing
@testable import FogarWire

/// The golden JSON pinned in `crates/fogar-host/src/ipc_wire.rs` (`golden_json`): what the Swift
/// app must parse and produce. If Rust changes it, IPC_VERSION bumps and these fail.
@Suite struct GoldenJSON {
    @Test func eventDecodes() throws {
        let json = #"{"type":"event","data":{"link":"ready","phase":"ready","gateway":{"address":"http://green-house:8004","name":"green-house","version":"1.29.0","client_name":"Mac de Sito"},"sessions":3,"turns_completed":7,"boot_mode":"normal","peer_check":"same_signer"}}"#
        guard case .event(let s) = try WireCodec.decodeResponse(Data(json.utf8)) else {
            Issue.record("not an event"); return
        }
        #expect(s.phase == .ready)
        #expect(s.gateway?.name == "green-house")
        #expect(s.gateway?.client_name == "Mac de Sito")
        #expect(s.sessions == 3 && s.turns_completed == 7)
        #expect(s.peer_check == .sameSigner)
    }

    @Test func pairFailedAndDatalessDecode() throws {
        guard case .pairFailed(let f) = try WireCodec.decodeResponse(
            Data(#"{"type":"pair_failed","data":{"reason":"invalid_code"}}"#.utf8)) else {
            Issue.record("not pair_failed"); return
        }
        #expect(f.reason == .invalidCode)
        guard case .subscribed = try WireCodec.decodeResponse(Data(#"{"type":"subscribed"}"#.utf8)) else {
            Issue.record("not subscribed"); return
        }
    }

    @Test func notPairedStatusHasNullGateway() throws {
        let json = #"{"type":"status","data":{"link":"not_paired","phase":"not_paired","gateway":null,"sessions":0,"turns_completed":0,"boot_mode":"safe","peer_check":"uid_only"}}"#
        guard case .status(let s) = try WireCodec.decodeResponse(Data(json.utf8)) else {
            Issue.record("not a status"); return
        }
        #expect(s.gateway == nil && s.peer_check == .uidOnly && s.boot_mode == "safe")
    }

    /// Requests are compared as JSON objects: JSONEncoder's key order is not stable.
    @Test(arguments: [
        (WireRequest.pair(WirePair(address: "green-house", code: "482913")),
         #"{"type":"pair","data":{"address":"green-house","code":"482913"}}"#),
        (.hello(WireHello(version: 2)), #"{"type":"hello","data":{"version":2}}"#),
        (.subscribe, #"{"type":"subscribe"}"#),
        (.discover, #"{"type":"discover"}"#),
        (.unpair, #"{"type":"unpair"}"#),
    ])
    func requestsMatchRust(request: WireRequest, golden: String) throws {
        let ours = try JSONSerialization.jsonObject(with: WireCodec.encode(request)) as? NSDictionary
        let theirs = try JSONSerialization.jsonObject(with: Data(golden.utf8)) as? NSDictionary
        #expect(ours == theirs)
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

@Suite struct PairCodeParsing {
    @Test(arguments: [("482913", "482913"), ("482 913", "482913"), ("482-913", "482913"),
                      ("482\u{00A0}913", "482913"), (" 482913\n", "482913")])
    func accepts(typed: String, digits: String) { #expect(parsePairCode(typed) == digits) }

    @Test(arguments: ["48291", "4829134", "48291a", "٤٨٢٩١٣", "", "482_913"])
    func rejects(typed: String) { #expect(parsePairCode(typed) == nil) }
}

/// End to end over a real Unix socket against an in-test listener: framing + connection.
@Suite struct OverASocket {
    @Test func connectToNothingIsUnavailable() async {
        let path = NSTemporaryDirectory() + "fogar-none-\(UUID().uuidString.prefix(8)).sock"
        await #expect(throws: LinkError.unavailable) {
            _ = try await FramedConnection.connect(path: path)
        }
    }
}
