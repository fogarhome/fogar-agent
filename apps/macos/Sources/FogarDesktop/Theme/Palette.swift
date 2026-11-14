import AppKit
import SwiftUI

/// fogar-brand tokens (`marca/paleta.css`, «Lousa con humo de monte») plus the dashboard's own
/// (`web/src/styles/tokens.css`). Dark is the brand; the light values are a derived variant
/// (the owner chose «follow the system», 2026-11-14): proposed, not voted in fogar-brand, with every
/// text/state pair measured AA on `noche`, `piedra` and `piedraAlta`.
extension Color {
    // Materia
    static let noche = dyn(0x0B1012, 0xF1ECE3)
    static let piedra = dyn(0x12191B, 0xE7E1D6)
    static let piedraAlta = dyn(0x1A2325, 0xDDD6C9)
    static let veta = dyn(0x252F32, 0xCBC3B4)
    static let hueso = dyn(0xECE6DC, 0x0B1012)
    static let huesoSuave = dyn(0xB8BEC2, 0x353F43)
    static let huesoTenue = dyn(0x8B969F, 0x56616A)

    // Acción: el botón principal es la tinta invertida.
    static let accion = dyn(0xECE6DC, 0x0B1012)
    static let accionAlta = dyn(0xFFF8EE, 0x1F2A2D)
    static let sobreAccion = dyn(0x0B1012, 0xF1ECE3)
    static let velo = dyn(0xECE6DC, 0x0B1012, alpha: 0.08, lightAlpha: 0.06)
    static let veloAlto = dyn(0xECE6DC, 0x0B1012, alpha: 0.12, lightAlpha: 0.10)

    // Hilos del lienzo
    static let hilo = dyn(0x2C3739, 0xBDB5A6)
    static let hiloCaido = dyn(0x3A4549, 0xA79F90)

    // Estados: planos, siempre con su icono y su texto.
    static let correcto = dyn(0x78B8F0, 0x1D67A8)
    static let degradado = dyn(0xE6DF7A, 0x7A6C00)
    static let noDisponible = dyn(0x8F99A1, 0x5F6A72)
    static let error = dyn(0xFF8DB4, 0xB8255F)

    // Luz (solo línea viva, fondo vivo y el foco de lo que pasa ahora)
    static let rescoldo = Color(hex: 0x5A1A10)
    static let ascua = Color(hex: 0xB8361A)
    static let llama = Color(hex: 0xEE6A2C)
    static let chispa = Color(hex: 0xFFE1B8)

    /// `vidrio-fino`: the dashboard's node glass, thin enough for the living background to show.
    static let vidrioFino = dyn(0x12191B, 0xFBF8F2, alpha: 0.42, lightAlpha: 0.62)
    static let vidrioBorde = dyn(0xECE6DC, 0x0B1012, alpha: 0.08, lightAlpha: 0.10)
    static let vidrioReflejo = dyn(0xECE6DC, 0xFFFFFF, alpha: 0.10, lightAlpha: 0.7)

    init(hex: UInt32, alpha: Double = 1) {
        self.init(.sRGB, red: Double(hex >> 16 & 0xFF) / 255, green: Double(hex >> 8 & 0xFF) / 255,
                  blue: Double(hex & 0xFF) / 255, opacity: alpha)
    }

    private static func dyn(_ dark: UInt32, _ light: UInt32, alpha: Double = 1, lightAlpha: Double? = nil) -> Color {
        Color(nsColor: NSColor(name: nil) { appearance in
            let isDark = appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            let hex = isDark ? dark : light
            return NSColor(srgbRed: CGFloat(hex >> 16 & 0xFF) / 255, green: CGFloat(hex >> 8 & 0xFF) / 255,
                           blue: CGFloat(hex & 0xFF) / 255, alpha: isDark ? alpha : (lightAlpha ?? alpha))
        })
    }
}

/// Spacing, radii and timing from the dashboard tokens.
enum Medida {
    static let radioChip: CGFloat = 10
    static let radioNodo: CGFloat = 12
    static let radioBloque: CGFloat = 14
    static let radioVidrio: CGFloat = 18
    static let pulsar = 0.12
    static let rapido = 0.16
    static let base = 0.24
}
