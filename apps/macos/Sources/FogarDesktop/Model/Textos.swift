import FogarWire
import Foundation

/// Everything the app says, in one place. Spanish, warm and direct, and only what the agent knows.
/// Pair failures follow the gateway's client protocol §0 (one clear sentence each).
enum Textos {
    struct Titular: Equatable {
        let titulo: String
        let sub: String
        let tono: Tono
    }

    /// `retirado`: `AgentLink.accesoRetirado` (the gateway withdrew this Mac's access).
    static func titular(_ state: AgentState, retirado: Bool = false) -> Titular {
        switch state {
        case .looking:
            return Titular(titulo: "Buscando al agente…", sub: "Un momento: la app está llamando al agente de este Mac.", tono: .off)
        case .notRunning:
            return Titular(titulo: "El agente no está en marcha",
                           sub: "Fogar Desktop habla con Fogar a través del agente de este Mac. Arráncalo y la app se conecta sola.",
                           tono: .bad)
        case .versionMismatch(let agente) where agente > ipcVersion:
            return Titular(titulo: "Actualiza Fogar Desktop",
                           sub: "El agente habla una versión más nueva (\(agente)) que esta app (\(ipcVersion)). Instala la app que va con ese agente.",
                           tono: .bad)
        case .versionMismatch(let agente):
            return Titular(titulo: "Actualiza el agente de Fogar",
                           sub: "Esta app habla la versión \(ipcVersion) y el agente, la \(agente). Instala el agente que va con esta app.",
                           tono: .bad)
        case .noLiveStatus:
            return Titular(titulo: "Actualiza el agente de Fogar",
                           sub: "Este agente aún no cuenta su estado en vivo a la app. Instala el agente que va con esta versión de Fogar Desktop.",
                           tono: .bad)
        case .unintelligible:
            return Titular(titulo: "La app no entiende al agente",
                           sub: "El agente ha contado algo que esta app no sabe leer. Seguramente uno de los dos está desactualizado: actualiza los dos.",
                           tono: .bad)
        case .noVerificado:
            return Titular(titulo: "No parece el agente de Fogar",
                           sub: "Algo responde en el sitio del agente, pero no está firmado como esta app. Por seguridad, la app no le cuenta nada. Reinstala Fogar Desktop con su agente.",
                           tono: .bad)
        case .refused:
            return Titular(titulo: "El agente no deja entrar a esta app",
                           sub: "El agente ha rechazado la conexión. Comprueba que la app y el agente son de la misma instalación.",
                           tono: .bad)
        case .live(let s):
            let nombre = s.gateway?.name
            switch s.phase {
            case .ready:
                return Titular(titulo: "Conectado a \(nombre ?? "tu gateway")",
                               sub: "Este Mac está unido a Fogar. Si el gateway se reinicia, vuelve solo.", tono: .ok)
            case .connecting:
                return Titular(titulo: nombre.map { "Conectando con \($0)…" } ?? "Conectando…",
                               sub: "El agente está abriendo la sesión con el gateway.", tono: .off)
            case .reconnecting:
                return Titular(titulo: nombre.map { "Reconectando con \($0)" } ?? "Reconectando",
                               sub: "El gateway no responde. El agente lo vuelve a intentar solo, cada vez más espaciado.", tono: .warn)
            case .notPaired where retirado:
                // Gateway M19 close 4200 (or 4010): the agent deleted the dead key. Without this
                // the person lands on «Hola. Emparejemos este Mac» with no idea why.
                return Titular(titulo: "Se retiró el acceso a este Mac",
                               sub: "El gateway ya no reconoce a este Mac: le han retirado el acceso. Para volver a usar Fogar aquí, abre un código nuevo en el gateway y empareja este Mac otra vez.",
                               tono: .bad)
            case .notPaired:
                return Titular(titulo: "Este Mac aún no está emparejado",
                               sub: "Empareja este Mac con el gateway de tu casa para que Fogar lo reconozca.", tono: .off)
            case .parked:
                return parked(s.link, nombre)
            case .stopping:
                return Titular(titulo: "El agente se está deteniendo", sub: "Cuando vuelva a arrancar, la app se conecta sola.", tono: .off)
            }
        }
    }

    /// `parked_*`: retrying would loop, so the agent waits (LinkState::Parked).
    private static func parked(_ link: String, _ nombre: String?) -> Titular {
        let gw = nombre ?? "el gateway"
        switch link {
        case "parked_agent_refused":
            return Titular(titulo: "El gateway no acepta a este Mac",
                           sub: "El gateway rechaza el agente que pide este Mac. Revisa la ficha de este Mac en el gateway; después pulsa Reconectar.",
                           tono: .bad)
        case "parked_repair":
            return Titular(titulo: "Hay que volver a emparejar este Mac",
                           sub: "\(nombre.map { "El gateway \($0)" } ?? "El gateway") ya no reconoce la clave de este Mac. Desemparéjalo en Ajustes y emparéjalo con un código nuevo.",
                           tono: .bad)
        case "parked_conversation_taken":
            // Gateway M19 close 4102: reconnecting by itself would evict the other session, which
            // would evict this one back (spec §13), so only the person's «Reconectar» does it.
            return Titular(titulo: "Otra sesión abrió la conversación de este Mac",
                           sub: "Algo más (otra pestaña del panel u otro cliente) abrió en \(gw) la conversación que usaba este Mac, así que este Mac se ha apartado. Pulsa Reconectar cuando quieras volver: la otra sesión se cerrará.",
                           tono: .bad)
        case "parked_upgrade_client":
            // Gateway 5.0 close 4003: this client's protocol is not supported; retrying by itself never helps; the person may press Reconectar.
            return Titular(titulo: "Hay que actualizar Fogar Desktop",
                           sub: "El gateway habla una versión nueva. Actualiza Fogar Desktop; si ya lo hiciste, pulsa Reconectar.",
                           tono: .bad)
        default:
            return Titular(titulo: "El agente se ha parado",
                           sub: "La conexión con \(gw) falló por un error del propio agente. Reiniciar el agente puede arreglarlo; si vuelve a pasar, hay que corregirlo.",
                           tono: .bad)
        }
    }

    /// What the gateway node says under its name.
    static func estadoGateway(_ s: WireStatus) -> String {
        let v = s.gateway.map { "\($0.version) · " } ?? ""
        switch s.phase {
        case .ready: return v + "conectado"
        case .connecting: return v + "conectando"
        case .reconnecting: return v + "no responde, reintentando"
        case .notPaired: return "sin emparejar"
        case .parked: return v + "en pausa"
        case .stopping: return v + "deteniéndose"
        }
    }

    static func autenticacion(_ check: WirePeerCheck) -> String {
        switch check {
        case .sameSigner: "Firmada por el mismo firmante que el agente"
        case .uidOnly: "Solo por tu usuario del Mac (sin comprobar la firma)"
        }
    }

    /// The short form under the app's node.
    static func autenticacionCorta(_ check: WirePeerCheck) -> String {
        switch check {
        case .sameSigner: "firmada como el agente"
        case .uidOnly: "solo tu usuario, sin comprobar la firma"
        }
    }

    /// What the hub says about the agent itself.
    static func agente(_ state: AgentState) -> String {
        switch state {
        case .live: "Agente · en marcha"
        case .looking: "Agente · llamando…"
        case .notRunning: "Agente · sin respuesta"
        case .versionMismatch: "Agente · otra versión"
        case .refused: "Agente · rechaza la app"
        case .noLiveStatus, .unintelligible: "Agente · desactualizado"
        case .noVerificado: "Agente · sin verificar"
        }
    }

    static func arranque(_ mode: String) -> String {
        switch mode {
        case "normal": "Normal"
        case "safe": "Modo seguro"
        default: mode
        }
    }

    /// One sentence per `pair_failed` reason (gateway client protocol §0).
    static func fallo(_ f: WirePairFailure) -> String {
        switch f {
        case .badAddress: "Esa dirección no parece válida. Escribe algo como gateway.local o 192.168.0.10:8004."
        case .badCode: "El código son 6 cifras. Revísalo."
        case .unreachable: "No llego a esa dirección. Comprueba que el gateway está encendido y en tu red o en Tailscale."
        case .notAGateway: "En esa dirección responde algo, pero no es un gateway de Fogar."
        case .invalidCode: "Ese código no vale. Si estás seguro, pide uno nuevo."
        case .gatewayError: "El gateway ha fallado al emparejar. Prueba otra vez con el mismo código."
        case .storageFailed: "No he podido guardar la clave en el llavero de este Mac. Pide un código nuevo y prueba otra vez."
        case .alreadyPaired: "Este Mac ya está emparejado. Desemparéjalo primero en Ajustes."
        case .busy: "Ya hay un emparejamiento en marcha. Espera a que termine."
        }
    }

    static func fallo(_ e: CommandError) -> String {
        switch e {
        case .agentNotRunning: "El agente no está en marcha. Arráncalo y vuelve a probar."
        case .versionMismatch: "El agente habla otra versión que esta app. Actualiza el que esté atrasado."
        case .refused: "El agente no deja hacer esto desde esta app: no está firmada por el mismo firmante que el agente."
        case .noAnswer: "El agente no ha contestado a tiempo. Prueba otra vez."
        case .unexpected: "El agente ha contestado algo que esta app no esperaba. Prueba otra vez."
        case .notOurAgent: "Lo que responde en el sitio del agente no está firmado como esta app: no le he enviado nada."
        }
    }

    /// `refused` on reconnect: either the pause ended meanwhile or this app is not signed.
    static func falloReconectar(_ e: any Error) -> String {
        switch (e as? CommandError) ?? .unexpected {
        case .refused: "El agente no ha reconectado: o ya no estaba en pausa, o esta app no está firmada por el mismo firmante que el agente."
        case let other: fallo(other)
        }
    }

    /// The search for gateways is open to any app: a refusal there is not about signing.
    static func falloBusqueda(_ e: CommandError) -> String {
        switch e {
        case .refused: "Este agente todavía no sabe buscar gateways. Escribe la dirección aquí abajo."
        default: fallo(e)
        }
    }

    static func fallo(_ e: any Error) -> String {
        fallo((e as? CommandError) ?? .unexpected)
    }

    /// «desde hace 5 min»
    static func desde(_ date: Date, ahora: Date = Date()) -> String {
        let s = max(0, Int(ahora.timeIntervalSince(date)))
        switch s {
        case ..<60: return "desde hace un momento"
        case ..<3600: return "desde hace \(s / 60) min"
        case ..<86_400: return "desde hace \(s / 3600) h \(s % 3600 / 60) min"
        default: return "desde hace \(s / 86_400) d"
        }
    }
}
