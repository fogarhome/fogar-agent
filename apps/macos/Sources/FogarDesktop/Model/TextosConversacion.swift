import FogarWire
import Foundation

/// What the conversation says. Same voice as the rest of `Textos`: Spanish, short, only what the
/// agent knows.
extension Textos {
    /// `refused` to a conversation request: the agent's command queue was full (busy), or the
    /// app is not the signed one.
    static let conversacionOcupada = "El agente no ha aceptado esto ahora. Prueba otra vez en un momento."

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

    static func rechazoTexto(_ r: WireTextRefusal) -> String {
        switch r {
        case .empty: "Escribe algo antes de enviarlo."
        case .tooLong: "Es demasiado largo para un mensaje. Pártelo en dos."
        case .notReady: "Fogar no está conectado al gateway ahora. Tu texto sigue en la caja."
        case .recording: "Termina de grabar antes de enviar texto."
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
        case .listo: "Pulsa para grabar y revisar, o mantén pulsado y suelta para enviar."
        case .sinConexion: "Sin conexión con Fogar: ahora no se puede grabar."
        case .soloAppFirmada: "La conversación solo funciona con la app de Fogar firmada como el agente."
        case .sinTranscriptor: "El gateway no tiene transcriptor ahora. Puedes escribir."
        case .pedirPermiso: "Fogar necesita tu permiso para usar el micrófono."
        case .denegado: microDenegado
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
        case .tool: herramienta.map { "Usando \($0)…" } ?? "Usando una herramienta…"
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
        case .error?: t.error_code.map { "Algo falló en el gateway (\($0))." } ?? "Algo falló en el gateway."
        }
    }

    static let pushEtiqueta = "Fogar te escribió"
    static let pushSinSonido = "Sin sonido: no tenías la conversación delante."
    static let otroDia = "Otro día"
}
