import Darwin
import Foundation
import Testing
@testable import FogarWire

/// The app's peer check against real signed processes on a real socket (review of #23). The
/// agents are signed in CI by `scripts/ci-signed-agents.sh` with a temporary self-signed
/// certificate; without its variables (a laptop, Linux) the suite does not run.
@Suite(.enabled(if: ProcessInfo.processInfo.environment["FOGAR_CI_SIGNED_AGENTS"] != nil))
struct SignedAgentTests {
    let env = ProcessInfo.processInfo.environment
    var dir: String { env["FOGAR_CI_SIGNED_AGENTS"] ?? "" }
    var equipo: String { env["FOGAR_CI_TEST_TEAM"] ?? "" }
    /// The temporary certificate stands in for Apple's anchor.
    var ancla: String { "certificate root = H\"\(env["FOGAR_CI_TEST_CERT_SHA1"] ?? "")\"" }

    /// Starts `agent-<nombre>`, connects to it as the app does, and runs the check on that fd.
    private func verifica(_ nombre: String, equipo: String? = nil, ancla: String? = nil) throws -> Bool {
        let sock = "/tmp/fogar-pc-\(nombre)-\(UUID().uuidString.prefix(8)).sock"
        unlink(sock)
        let p = Process()
        p.executableURL = URL(fileURLWithPath: dir + "/agent-" + nombre)
        p.arguments = [sock]
        try p.run()
        defer {
            p.terminate()
            p.waitUntilExit()
            unlink(sock)
        }
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        try #require(fd >= 0)
        defer { close(fd) }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &addr.sun_path) { buf in
            buf.copyBytes(from: sock.utf8.prefix(buf.count - 1))
        }
        let len = socklen_t(MemoryLayout<sockaddr_un>.size)
        var conectado = false
        for _ in 0..<100 where !conectado {
            conectado = withUnsafePointer(to: &addr) {
                $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, len) == 0 }
            }
            if !conectado { usleep(50_000) }
        }
        try #require(conectado, "agent-\(nombre) never listened")
        // Its first byte: the server has done I/O on this connection.
        var b: UInt8 = 0
        try #require(read(fd, &b, 1) == 1)
        return PeerCheck.servidorValido(
            fd: fd, verificacion: .agenteFirmado, equipo: equipo ?? self.equipo, ancla: ancla ?? self.ancla)
    }

    @Test func aceptaAlAgenteFirmadoComoComFogarAgent() throws {
        #expect(try verifica("ok"))
    }

    @Test func rechazaOtroIdentificador() throws {
        #expect(try !verifica("otro"))
    }

    @Test func rechazaSinHardenedRuntime() throws {
        #expect(try !verifica("sinruntime"))
    }

    @Test func rechazaUnAgenteDepurable() throws {
        #expect(try !verifica("depurable"))
    }

    @Test func rechazaOtroEquipo() throws {
        #expect(try !verifica("ok", equipo: "OTROEQUIPO"))
    }

    /// The production anchor: a certificate that is not Apple's never passes.
    @Test func elAnclaRealRechazaUnCertificadoQueNoEsDeApple() throws {
        #expect(try !verifica("ok", ancla: PeerCheck.anclaApple))
    }

    /// Without a Team of its own (ad hoc or unsigned app), nothing is trusted.
    @Test func sinEquipoPropioNadaSeAcepta() throws {
        #expect(try !verificaSinEquipo())
    }

    private func verificaSinEquipo() throws -> Bool {
        // Same connection path, no Team: the check refuses before looking at the signature.
        var fds: [Int32] = [0, 0]
        try #require(socketpair(AF_UNIX, SOCK_STREAM, 0, &fds) == 0)
        defer { close(fds[0]); close(fds[1]) }
        return PeerCheck.servidorValido(fd: fds[0], verificacion: .agenteFirmado, equipo: nil, ancla: ancla)
    }
}

/// Runs everywhere: the requirement the app builds names the agent and forbids what would let
/// another process speak for it.
@Suite struct RequisitoAgente {
    /// In GitHub Actions the signed agents must exist: a silently skipped suite proves nothing.
    @Test func enCIHayAgentesFirmados() {
        let env = ProcessInfo.processInfo.environment
        if env["GITHUB_ACTIONS"] == "true" { #expect(env["FOGAR_CI_SIGNED_AGENTS"] != nil) }
    }

    @Test func requisito() {
        let r = PeerCheck.requisitoAgente(equipo: "Q6UYYP85B7", ancla: PeerCheck.anclaApple)
        #expect(r.hasPrefix("anchor apple generic and certificate leaf[subject.OU] = \"Q6UYYP85B7\""))
        #expect(r.contains("identifier \"com.fogar.agent\""))
        for e in ["get-task-allow", "cs.disable-library-validation", "cs.allow-dyld-environment-variables"] {
            #expect(r.contains("!(entitlement[\"com.apple.security.\(e)\"] exists)"))
        }
    }
}
