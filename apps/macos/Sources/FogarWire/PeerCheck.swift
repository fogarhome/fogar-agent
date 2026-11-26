import Darwin
import Foundation
import Security

/// How the app authenticates the server behind the agent's socket (the mirror of the agent's own
/// peer check, ADR-0002): any same-user process could otherwise bind a socket there and read
/// what the person types and says.
public enum VerificacionPar: Sendable, Equatable {
    /// The run directory is ours (0700, no symlink), the server runs as our user, and its code is
    /// signed by this app's own Team with the agent's identifier and the hardened runtime, not
    /// debuggable. Required for everything that carries the person's content or changes where it
    /// goes: the conversation, `login` (it carries the password), `logout`, `reconnect`.
    case agenteFirmado
    /// Only the directory and user checks. Enough for the read-only status (`hello`, `status`,
    /// `subscribe`, `discover`), so an agent not yet packaged and signed (S1, until S2 PR 6) still
    /// shows its state. Debug builds use it for everything only with `FOGAR_AGENT_UNSIGNED=1`.
    case rutaYUsuario
}

/// The agent's code-signing identifier: `scripts/build-app.sh` signs `Contents/MacOS/fogar-agentd`
/// with it (and embeds it as the agent's `CFBundleIdentifier`). Frozen identity.
public let identificadorAgente = "com.fogar.agent"

public enum PeerCheck {
    /// `path` is a socket we own, in a directory we own with mode 0700, neither a symlink.
    public static func rutaPropia(_ path: String) -> Bool {
        let uid = getuid()
        var s = stat()
        guard lstat(path, &s) == 0, s.st_mode & S_IFMT == S_IFSOCK, s.st_uid == uid else { return false }
        let dir = (path as NSString).deletingLastPathComponent
        var d = stat()
        guard lstat(dir, &d) == 0, d.st_mode & S_IFMT == S_IFDIR, d.st_uid == uid else { return false }
        return d.st_mode & 0o777 == 0o700
    }

    /// The process behind the connected socket `fd` is our agent (see `VerificacionPar`).
    public static func servidorValido(fd: Int32, verificacion: VerificacionPar) -> Bool {
        servidorValido(fd: fd, verificacion: verificacion, equipo: equipoPropio, ancla: anclaApple)
    }

    /// Apple-issued certificates only. Tests replace it with their temporary certificate's hash
    /// (CI signs test agents with a self-signed certificate: `scripts/ci-signed-agents.sh`).
    static let anclaApple = "anchor apple generic"

    static func servidorValido(fd: Int32, verificacion: VerificacionPar, equipo: String?, ancla: String) -> Bool {
        // uid gate (`getpeereid` reads LOCAL_PEERCRED).
        var uid: uid_t = 0
        var gid: gid_t = 0
        guard getpeereid(fd, &uid, &gid) == 0, uid == getuid() else { return false }
        if verificacion == .rutaYUsuario { return true }
        guard let equipo, let token = tokenDelPar(fd) else { return false }
        return cumple(token: token, requisito: requisitoAgente(equipo: equipo, ancla: ancla))
    }

    /// The agent's designated requirement for Team `equipo` (already validated: ten upper-case
    /// letters and digits).
    static func requisitoAgente(equipo: String, ancla: String) -> String {
        "\(ancla) and certificate leaf[subject.OU] = \"\(equipo)\" and identifier \"\(identificadorAgente)\""
            + " and !(entitlement[\"com.apple.security.cs.allow-dyld-environment-variables\"] exists)"
            + " and !(entitlement[\"com.apple.security.cs.disable-library-validation\"] exists)"
            + " and !(entitlement[\"com.apple.security.get-task-allow\"] exists)"
    }

    /// `LOCAL_PEERTOKEN`: the audit token of the process that last did I/O on the peer socket.
    static func tokenDelPar(_ fd: Int32) -> Data? {
        let solLocal: Int32 = 0 // SOL_LOCAL
        let localPeerToken: Int32 = 0x006 // LOCAL_PEERTOKEN
        var token = audit_token_t()
        var len = socklen_t(MemoryLayout<audit_token_t>.size)
        guard getsockopt(fd, solLocal, localPeerToken, &token, &len) == 0,
              len == socklen_t(MemoryLayout<audit_token_t>.size) else { return nil }
        return withUnsafeBytes(of: &token) { Data($0) }
    }

    /// This app's own Team ID (none for an ad-hoc or unsigned build: then nothing is trusted).
    /// Read once: the app's own signature does not change while it runs.
    static let equipoPropio: String? = leerEquipoPropio()

    private static func leerEquipoPropio() -> String? {
        var yo: SecCode?
        guard SecCodeCopySelf([], &yo) == errSecSuccess, let yo else { return nil }
        var estatico: SecStaticCode?
        guard SecCodeCopyStaticCode(yo, [], &estatico) == errSecSuccess, let estatico else { return nil }
        var info: CFDictionary?
        guard SecCodeCopySigningInformation(estatico, SecCSFlags(rawValue: kSecCSSigningInformation), &info) == errSecSuccess,
              let d = info as? [String: Any],
              let equipo = d[kSecCodeInfoTeamIdentifier as String] as? String else { return nil }
        // Ten upper-case letters and digits: nothing that could change the requirement's meaning.
        guard equipo.count == 10, equipo.unicodeScalars.allSatisfy({ ("A"..."Z").contains($0) || ("0"..."9").contains($0) }) else {
            return nil
        }
        return equipo
    }

    /// The running code behind `token` satisfies `requisito` and runs with the hardened runtime.
    static func cumple(token: Data, requisito: String) -> Bool {
        let attrs = [kSecGuestAttributeAudit as String: token] as CFDictionary
        var guest: SecCode?
        guard SecCodeCopyGuestWithAttributes(nil, attrs, [], &guest) == errSecSuccess, let guest else { return false }
        var req: SecRequirement?
        guard SecRequirementCreateWithString(requisito as CFString, [], &req) == errSecSuccess, let req else { return false }
        guard SecCodeCheckValidity(guest, [], req) == errSecSuccess else { return false }
        var info: CFDictionary?
        let estatico = unsafeBitCast(guest, to: SecStaticCode.self)
        guard SecCodeCopySigningInformation(estatico, SecCSFlags(rawValue: kSecCSDynamicInformation), &info) == errSecSuccess,
              let d = info as? [String: Any],
              let estado = d[kSecCodeInfoStatus as String] as? NSNumber else { return false }
        // CS_RUNTIME (0x10000) set, CS_DEBUGGED (0x10000000) not: a debugger attached to the
        // agent could make it say anything.
        return estado.uint32Value & 0x10000 != 0 && estado.uint32Value & 0x1000_0000 == 0
    }
}
