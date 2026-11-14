import AppKit
import SwiftUI

/// The living background (fogar-brand FondoVivo, panel mode, as in the dashboard's FondoVivo.vue):
/// green mountain smoke and ember moving slowly, painted into a 112 × 64 bitmap and scaled up, 3 %
/// grain, faded into the night by an elliptical mask. The ember is light (added) and never passes
/// 42 % under text. Still under reduce motion; no work at all while the window is hidden.
struct FondoVivo: View {
    var calor = 0.38
    var humo = 0.75
    /// A still frame, never animated (the menu bar extra).
    var quieto = false

    @Environment(\.accessibilityReduceMotion) private var reduce
    @Environment(\.colorScheme) private var scheme
    @Environment(\.ventanaVisible) private var visible
    @Environment(\.controlActiveState) private var activa

    var body: some View {
        // Moves only while you look at it: window on screen and the app in front. The smoke is slow
        // (velocity 0.4), so 12 frames a second read as continuous.
        CapaFondo(calor: calor, humo: humo, velocidad: 0.4, oscuro: scheme == .dark,
                  animado: !(quieto || reduce || !visible || activa == .inactive))
            // The grain is a static layer of its own (never redrawn per frame), faded like the smoke.
            .overlay(Grano().opacity(0.03).mask(FondoVivo.mascara))
            .allowsHitTesting(false)
            .accessibilityHidden(true)
    }
}

/// The smoke in its own Core Animation layer: each frame only swaps the layer's 112 × 64 contents
/// and the GPU scales it. Drawing it through SwiftUI made the whole screen re-render 12 times a
/// second (~10 % CPU on a large window); this way the rest of the view tree is untouched.
private struct CapaFondo: NSViewRepresentable {
    let calor: Double, humo: Double, velocidad: Double, oscuro: Bool, animado: Bool

    func makeNSView(context: Context) -> Vista { Vista() }

    func updateNSView(_ v: Vista, context: Context) {
        v.parametros = (calor, humo, velocidad, oscuro)
        v.animado = animado
        v.pintar()
    }

    final class Vista: NSView {
        var parametros = (0.38, 0.75, 0.4, true)
        var animado = false { didSet { if animado != oldValue { programar() } } }
        private var reloj: Timer?
        /// Time keeps running only while animated, so pausing and resuming never jumps.
        private var t = 0.0, ultimo: Date?

        override init(frame: NSRect) {
            super.init(frame: frame)
            wantsLayer = true
            layer?.contentsGravity = .resize
            layer?.magnificationFilter = .linear
        }

        required init?(coder: NSCoder) { nil }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            programar()
        }

        private func programar() {
            reloj?.invalidate()
            reloj = nil
            ultimo = nil
            guard animado, window != nil else { return }
            let r = Timer(timeInterval: 1.0 / 12, repeats: true) { [weak self] _ in
                MainActor.assumeIsolated { self?.pintar() }
            }
            r.tolerance = 0.02 // lets the system coalesce wake-ups
            RunLoop.main.add(r, forMode: .common)
            reloj = r
        }

        func pintar() {
            let ahora = Date()
            if animado, let u = ultimo { t += ahora.timeIntervalSince(u) }
            ultimo = animado ? ahora : nil
            let (c, h, v, o) = parametros
            layer?.contents = FondoPintor.pinta(t: t, calor: c, humo: h, velocidad: v, oscuro: o)
        }

    }
}

extension FondoVivo {
    /// `radial-gradient(closest-side, #000 55%, transparent 100%)`
    static let mascara = EllipticalGradient(stops: [.init(color: .black, location: 0), .init(color: .black, location: 0.55),
                                                    .init(color: .clear, location: 1)], endRadiusFraction: 0.5)
}

private struct Grano: View {
    static let textura: NSImage = {
        let n = 128
        var px = [UInt8](repeating: 255, count: n * n * 4)
        var s: UInt64 = 11
        for i in stride(from: 0, to: px.count, by: 4) {
            s = (s * 16807) % 2_147_483_647
            let v = UInt8(s % 256)
            px[i] = v; px[i + 1] = v; px[i + 2] = v
        }
        let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: n, pixelsHigh: n, bitsPerSample: 8,
                                   samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                                   bytesPerRow: n * 4, bitsPerPixel: 32)
        if let dst = rep?.bitmapData { px.withUnsafeBufferPointer { src in
            for i in 0..<src.count { dst[i] = src[i] }
        } }
        let img = NSImage(size: NSSize(width: n, height: n))
        if let rep { img.addRepresentation(rep) }
        return img
    }()

    var body: some View {
        Image(nsImage: Self.textura).resizable(resizingMode: .tile)
    }
}

/// Paints one frame: the dashboard's `pinta()`, literally, into a tiny CGContext.
enum FondoPintor {
    private static let lw = 112, lh = 64
    private static let space = CGColorSpace(name: CGColorSpace.sRGB) ?? CGColorSpaceCreateDeviceRGB()

    private typealias RGB = (Double, Double, Double)
    private static let rescoldo: RGB = (90, 26, 16), ascua: RGB = (184, 54, 26), llama: RGB = (238, 106, 44)
    private static let chispa: RGB = (255, 225, 184), contra: RGB = (124, 199, 146)
    private static let tope = 0.42

    static func pinta(t: Double, calor: Double, humo: Double, velocidad: Double, oscuro: Bool) -> CGImage? {
        guard let x = CGContext(data: nil, width: lw, height: lh, bitsPerComponent: 8, bytesPerRow: 0, space: space,
                                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        // Canvas coordinates: y down, like the web.
        x.translateBy(x: 0, y: CGFloat(lh))
        x.scaleBy(x: 1, y: -1)
        let W = Double(lw), H = Double(lh), s = t * velocidad
        // Materia y humo: dark = the brand; light = the same smoke, as pale mist over bone.
        let noche: RGB = oscuro ? (11, 16, 18) : (241, 236, 227)
        let humoC: RGB = oscuro ? (19, 32, 27) : (226, 230, 216)
        let niebla: RGB = oscuro ? (32, 51, 42) : (216, 226, 210)
        x.setFillColor(color(noche, 1))
        x.fill(CGRect(x: 0, y: 0, width: W, height: H))
        for k in 0..<3 {
            let kd = Double(k)
            let X = W * (0.2 + 0.6 * (0.5 + 0.5 * sin(s * 0.21 + kd * 2.1)))
            let Y = H * (0.15 + 0.55 * (0.5 + 0.5 * sin(s * 0.17 + kd * 1.3)))
            let r = W * (0.42 + 0.08 * sin(s * 0.3 + kd))
            radial(x, X, Y, r, [(k == 1 ? niebla : humoC, 0.75 * humo * (oscuro ? 1 : 0.7), 0), (k == 1 ? niebla : humoC, 0, 1)])
        }
        // Light adds on the night. On bone, adding light would vanish and multiplying reads as
        // dirt, so the ember is laid over thinly: a faint warmth, never a grey stain.
        x.setBlendMode(oscuro ? .plusLighter : .normal)
        let luz = oscuro ? 1.0 : 0.3
        let X0 = W * (0.5 + 0.45 * sin(s * 0.09 + 1)), Y0 = H * (0.25 + 0.2 * sin(s * 0.13))
        radial(x, X0, Y0, W * 0.5, [(contra, 0.07 * humo * luz, 0), (contra, 0, 1)])
        for k in 0..<3 {
            let kd = Double(k)
            let X = W * (0.18 + 0.64 * (0.5 + 0.5 * sin(s * (0.11 + 0.04 * kd) + kd * 2.3)))
            let Y = H * (0.8 + 0.18 * sin(s * (0.23 + 0.05 * kd) + kd))
            let r = W * (0.26 + 0.08 * sin(s * 0.6 + kd * 1.7))
            let fl = 0.8 + 0.2 * sin(s * 1.3 + kd * 2) * sin(s * 0.41 + kd)
            let a = calor * fl * luz
            let tono = 0.5 + 0.5 * sin(s * 0.35 + kd * 2.1)
            let nucleo: RGB = (ascua.0 + (llama.0 - ascua.0) * tono, ascua.1 + (llama.1 - ascua.1) * tono, ascua.2 + (llama.2 - ascua.2) * tono)
            radial(x, X, Y, r, [(nucleo, tope * a, 0), (rescoldo, 0.28 * a, 0.5), (rescoldo, 0, 1)])
            radial(x, X, Y, r * 0.4, [(chispa, 0.1 * a * tono, 0), (chispa, 0, 1)])
        }
        // The elliptical fade into the night, baked into the bitmap: compositing a mask over a
        // scaled image every frame cost more than painting the frame.
        x.setBlendMode(.destinationIn)
        x.saveGState()
        x.scaleBy(x: 1, y: H / W)
        radial(x, W / 2, W / 2, W / 2, [(noche, 1, 0), (noche, 1, 0.55), (noche, 0, 1)], hastaElBorde: true)
        x.restoreGState()
        return x.makeImage()
    }

    private static func color(_ c: RGB, _ a: Double) -> CGColor {
        CGColor(srgbRed: c.0 / 255, green: c.1 / 255, blue: c.2 / 255, alpha: a)
    }

    /// `hastaElBorde`: also paints past the end radius with the last stop (needed by the mask:
    /// destination-in keeps whatever the gradient does not cover).
    private static func radial(_ x: CGContext, _ cx: Double, _ cy: Double, _ r: Double, _ stops: [(RGB, Double, Double)],
                               hastaElBorde: Bool = false) {
        guard let g = CGGradient(colorsSpace: space, colors: stops.map { color($0.0, $0.1) } as CFArray,
                                 locations: stops.map { CGFloat($0.2) }) else { return }
        x.drawRadialGradient(g, startCenter: CGPoint(x: cx, y: cy), startRadius: 0,
                             endCenter: CGPoint(x: cx, y: cy), endRadius: r,
                             options: hastaElBorde ? [.drawsAfterEndLocation] : [])
    }
}

// MARK: Window visibility (no animation work while the window is hidden, closed or occluded)

private struct VentanaVisibleKey: EnvironmentKey { static let defaultValue = true }

extension EnvironmentValues {
    var ventanaVisible: Bool {
        get { self[VentanaVisibleKey.self] }
        set { self[VentanaVisibleKey.self] = newValue }
    }
}

extension View {
    /// Publishes whether this view's window is on screen (`NSWindow.occlusionState`) to the subtree.
    func vigilaVisibilidad() -> some View { modifier(VigilaVisibilidad()) }
}

private struct VigilaVisibilidad: ViewModifier {
    @State private var visible = true
    func body(content: Content) -> some View {
        content
            .environment(\.ventanaVisible, visible)
            .background(Sonda(visible: $visible).frame(width: 0, height: 0))
    }
}

private struct Sonda: NSViewRepresentable {
    @Binding var visible: Bool

    func makeNSView(context: Context) -> SondaView {
        let v = SondaView()
        v.cambio = { visible = $0 }
        return v
    }

    func updateNSView(_ nsView: SondaView, context: Context) {}

    final class SondaView: NSView {
        var cambio: (Bool) -> Void = { _ in }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            NotificationCenter.default.removeObserver(self)
            guard let window else { cambio(false); return }
            // Selector-based: unregistered automatically when the view goes away.
            NotificationCenter.default.addObserver(self, selector: #selector(informar),
                                                   name: NSWindow.didChangeOcclusionStateNotification, object: window)
            informar()
        }

        @objc private func informar() {
            cambio(window?.occlusionState.contains(.visible) ?? false)
        }
    }
}
