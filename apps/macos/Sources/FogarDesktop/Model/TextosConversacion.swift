import FogarWire
import Foundation

/// What the conversation says. Same voice as the rest of `Textos`: Spanish, short, only what the
/// agent knows.
extension Textos {
    /// `refused` to a conversation request, by the agent's reason (`fogar-core::ipc`).
    static func rechazo(_ motivo: String) -> String {
        switch motivo {
        case "engine busy": "El agente está ocupado ahora mismo. Prueba otra vez en un momento."
        case "needs the signed Fogar app": "El agente solo deja hacer esto a la app de Fogar firmada como él."
        case "unsupported request": "Este agente todavía no sabe hacer esto. Instala el agente que va con esta app."
        case "ipc version mismatch": "El agente habla otra versión que esta app. Actualiza el que esté atrasado."
        default: "El agente no ha aceptado esto. Prueba otra vez."
        }
    }

    static let tomaPerdida = "Se perdió la conexión con el agente y la grabación se descartó. Vuelve a grabar cuando reconecte."

    static let microDenegado = "Fogar no puede usar el micrófono. Ábrelo en Ajustes del Sistema → Privacidad y seguridad → Micrófono."

    static let corteSilencio = "He parado de grabar porque no te oía. Revisa el texto y envíalo."
    static let corteSeguridad = "He parado de grabar porque la toma era muy larga. Revisa el texto y envíalo."
    static let frasePerdida = "Se me ha escapado la última frase. ¿Me la repites?"

    static func aviso(_ n: WireNotice) -> String {
        switch n {
        case .silenceCut: corteSilencio
        case .safetyCut: corteSeguridad
        case .phraseLost: frasePerdida
        }
    }

    /// `CancelTurn` while the agent has no link: nothing could be asked to stop.
    static let cancelarSinConexion = "Fogar no está conectado: no se pudo parar."

    static func rechazoTexto(_ r: WireTextRefusal) -> String {
        switch r {
        case .empty: "Escribe algo antes de enviarlo."
        case .tooLong: "Es demasiado largo para un mensaje. Pártelo en dos."
        case .notReady: "Fogar no está conectado al gateway ahora. Tu texto sigue en la caja."
        case .recording: "Termina de grabar antes de enviar texto."
        case .missingScope: soloLectura
        }
    }

    static func rechazoToma(_ r: WireCaptureRefusal) -> String {
        switch r {
        case .notSubscribed: "La app ha perdido la conversación con el agente. Espera un momento y prueba otra vez."
        case .permissionNotDetermined: "Antes de grabar, permite el micrófono."
        case .permissionDenied: microDenegado
        case .noMicrophone: "Este Mac no tiene un micrófono que Fogar pueda usar."
        case .notReady: "Fogar no está conectado al gateway ahora. Puedes escribir cuando vuelva."
        case .noTranscriber: "El gateway no puede transcribir ahora. Escribe, que eso sí funciona."
        case .busy: "Ya hay una grabación en marcha."
        }
    }

    /// The microphone button's help and VoiceOver hint.
    static func micro(_ e: EstadoMicro) -> String {
        switch e {
        case .listo: "Pulsa para hablar: verás el texto mientras hablas."
        case .sinConexion: "Sin conexión con Fogar: ahora no se puede grabar."
        case .soloAppFirmada: "La conversación solo funciona con la app de Fogar firmada como el agente."
        case .sinTranscriptor: "Sin transcriptor: dictar llegará más adelante. De momento, escribe."
        case .pedirPermiso: "Fogar necesita tu permiso para usar el micrófono."
        case .denegado: microDenegado
        case .desconocido: "Fogar aún no sabe si puede usar el micrófono de este Mac. Puedes escribir."
        }
    }

    /// The line under the box when the conversation itself is not available.
    static func enlace(_ e: EnlaceConversacion, listo: Bool) -> String? {
        switch e {
        case .viva: listo ? nil : "Fogar no está conectado al gateway. Lo que escribas se queda en la caja hasta que vuelva."
        case .buscando: "Conectando con el agente…"
        case .sinAgente: "El agente no está en marcha. Arráncalo y la conversación vuelve sola."
        case .soloAppFirmada: "El agente solo abre la conversación a la app de Fogar firmada como él. Instala la app que va con el agente."
        case .noDisponible: "Esta app y el agente no se entienden: actualiza el que esté atrasado (mira Estado)."
        }
    }

    /// The progress line of an open turn.
    static func progreso(_ p: WireProgress, herramienta: String?) -> String {
        switch p {
        case .preparing: "Preparando…"
        case .thinking: "Pensando…"
        case .tool: herramienta.flatMap(nombreSeguro).map { "Usando \($0)…" } ?? "Usando una herramienta…"
        case .working: "Trabajando…"
        }
    }

    /// How a turn ended, when it is worth a line (`done` is not).
    static func desenlace(_ t: WireTurn) -> String? {
        switch t.outcome {
        case nil, .done?: nil
        case .cancelled?: "Cancelada."
        case .interrupted?: "Se cortó antes de terminar."
        case .replaced?: "Fogar pasó a tu siguiente mensaje."
        case .timeout?: "Fogar tardó demasiado y lo dejó."
        case .closed?: "Se cerró la conexión con el gateway."
        case .error?: t.error_code.flatMap(fraseOrquestador)
            ?? t.error_code.flatMap(nombreSeguro).map { "Algo falló en el gateway (\($0))." } ?? "Algo falló en el gateway."
        }
    }

    /// Gateway 5.0 orchestrator errors, each with its own phrase; any other code keeps the generic one.
    static func fraseOrquestador(_ code: String) -> String? {
        switch code {
        case "orchestrator_auth_failed": "El modelo de Fogar no tiene acceso ahora: lo tiene que arreglar quien administra Fogar."
        case "orchestrator_model_unavailable": "El modelo de Fogar no está disponible ahora."
        case "orchestrator_rate_limited": "El modelo está saturado o sin saldo. Prueba más tarde."
        case "orchestrator_timeout": "Fogar tardó demasiado en responder."
        case "orchestrator_agent_unavailable": "Este agente ya no está disponible para este Mac."
        default: nil
        }
    }

    /// A tool name or error code is shown only when it is a plain identifier (`[A-Za-z0-9_.:-]`,
    /// 1–64): anything else from the gateway could be text dressed up as the app's own words.
    static func nombreSeguro(_ s: String) -> String? {
        guard (1...64).contains(s.unicodeScalars.count),
              s.unicodeScalars.allSatisfy({ c in
                  ("a"..."z").contains(c) || ("A"..."Z").contains(c) || ("0"..."9").contains(c) || "_.:-".unicodeScalars.contains(c)
              })
        else { return nil }
        return s
    }

    static let pushEtiqueta = "Fogar te escribió"

    /// A conversation's name in the list (OpenClaw titles it from the first message).
    static func tituloConversacion(_ c: WireSummary) -> String {
        let t = c.title?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return t.isEmpty ? "Conversación sin título" : t
    }

    /// Who wrote a message, for VoiceOver.
    static func autorMensaje(_ m: WireMessage) -> String {
        if m.mine { return "Tú" }
        return m.from_name.flatMap(nombreDispositivo) ?? "Alguien de casa"
    }

    /// The line under a message, when there is something to say.
    static func notaMensaje(_ m: WireMessage) -> String? {
        switch m.state {
        case .pending: return "Pendiente: va cuando Fogar termine."
        case .failed:
            switch m.error_code {
            case "missing_scope": return "No se envió: tu cuenta no puede escribir."
            // Our deadline passed with no answer: the gateway may have it.
            case "unknown": return "Puede que se enviara: Fogar tardó en contestar. Mira si aparece antes de volver a escribirlo."
            default: return "No se envió. Escríbelo otra vez cuando Fogar vuelva."
            }
        case .delivered, .past:
            guard !m.mine, let nombre = m.from_name.flatMap(nombreDispositivo) else { return nil }
            return "Enviado desde \(nombre)"
        }
    }

    /// A device name is the person's own text (they named the device): shown as it is, cut to
    /// 40 characters, never empty.
    static func nombreDispositivo(_ s: String) -> String? {
        let t = s.trimmingCharacters(in: .whitespacesAndNewlines)
        return t.isEmpty ? nil : String(t.prefix(40))
    }
    static let soloLectura = "Tu cuenta puede leer las conversaciones, pero no escribir. Pídeselo a quien administra Fogar."
}
