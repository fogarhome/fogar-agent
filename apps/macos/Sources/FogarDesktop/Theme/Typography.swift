import AppKit
import CoreText
import SwiftUI

/// Unbounded titles (only titles), Figtree everything else, tabular figures for numbers that change
/// (fogar-brand `marca/tipografia.css`). Both are variable fonts bundled in Resources/Fonts (OFL).
enum Tipo {
    /// Registers the bundled fonts for this process. Missing files fall back to the system face.
    static func registrar() {
        guard let dir = Bundle.main.resourceURL?.appendingPathComponent("Fonts"),
              let files = try? FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)
        else { return }
        for url in files where url.pathExtension == "woff2" || url.pathExtension == "ttf" {
            CTFontManagerRegisterFontsForURL(url as CFURL, .process, nil)
        }
    }

    /// A variable-font instance at an exact `wght` (the brand uses 550, between medium and semibold).
    static func fuente(_ family: String, _ size: CGFloat, peso: CGFloat) -> Font {
        let wght: NSNumber = 0x7767_6874 // 'wght'
        let desc = CTFontDescriptorCreateWithAttributes([
            kCTFontFamilyNameAttribute: family,
            kCTFontVariationAttribute: [wght: peso],
        ] as CFDictionary)
        let font = CTFontCreateWithFontDescriptor(desc, size, nil)
        // Not registered (e.g. a test host): system face at the closest weight.
        guard (CTFontCopyFamilyName(font) as String) == family else {
            return .system(size: size, weight: peso >= 590 ? .semibold : peso >= 500 ? .medium : .regular)
        }
        return Font(font)
    }

    static func titular(_ size: CGFloat = 32) -> Font { fuente("Unbounded", size, peso: 400) }
    static func texto(_ size: CGFloat = 15, peso: CGFloat = 400) -> Font { fuente("Figtree", size, peso: peso) }

    static let h1 = titular(32)
    static let h2 = texto(17, peso: 600)
    static let sub = texto(16)
    static let cuerpo = texto(15)
    static let etiqueta = texto(14, peso: 550)
    static let meta = texto(13)
    static let cifra = texto(32, peso: 500)
}

extension View {
    /// `fg-h1`: Unbounded, tight tracking because it is wide.
    func titularFogar(_ size: CGFloat = 32) -> some View {
        font(Tipo.titular(size)).tracking(-0.035 * size).foregroundStyle(Color.hueso)
            .accessibilityAddTraits(.isHeader)
    }
}
