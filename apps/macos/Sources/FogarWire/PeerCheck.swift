import Darwin
import Foundation
import Security

/// How the app authenticates the server behind the agent's socket (the mirror of the agent's own
/// peer check, ADR-0002): any same-user process could otherwise bind a socket there and read
/// what the person types and says.
public enum VerificacionPar: Sendable, Equatable {
    /// Release: the run directory is ours (0700, no symlink), the server runs as our user, and its
    /// code is signed by this app's own Team with the agent's identifier and the hardened runtime.
    case agenteFirmado
    #if DEBUG
    /// Debug builds only, for the unsigned stand-in and the tests: the same directory and user
    /// checks, no code signature. Never compiled into a release build.
    case mismoUsuario
    #endif
}

/// The agent's code-signing identifier (GUESSED: the identifier the agent inside the `.app` is
/// signed with, set by the packaging of S2 PR 6; the agent's ADR-0002 note names it).
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
        // uid gate (`getpeereid` reads LOCAL_PEERCRED).
        var uid: uid_t = 0
        var gid: gid_t = 0
        guard getpeereid(fd, &uid, &gid) == 0, uid == getuid() else { return false }
        #if DEBUG
        if verificacion == .mismoUsuario { return true }
        #endif
        guard let equipo = equipoPropio(), let token = tokenDelPar(fd) else { return false }
        let requisito = "anchor apple generic and certificate leaf[subject.OU] = \"\(equipo)\" and identifier \"\(identificadorAgente)\""
            + " and !(entitlement[\"com.apple.security.cs.allow-dyld-environment-variables\"] exists)"
            + " and !(entitlement[\"com.apple.security.cs.disable-library-validation\"] exists)"
        return cumple(token: token, requisito: requisito)
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
    static func equipoPropio() -> String? {
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
        // Dynamic status: CS_RUNTIME (0x10000).
        var info: CFDictionary?
        let estatico = unsafeBitCast(guest, to: SecStaticCode.self)
        guard SecCodeCopySigningInformation(estatico, SecCSFlags(rawValue: kSecCSDynamicInformation), &info) == errSecSuccess,
              let d = info as? [String: Any],
              let estado = d[kSecCodeInfoStatus as String] as? NSNumber else { return false }
        return estado.uint32Value & 0x10000 != 0
    }
}
