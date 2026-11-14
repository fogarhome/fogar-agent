import AppKit
import SwiftUI

/// The brand in the interface: the line that writes «Fogar» and changes shape with the state.
/// 1000 × 420 canvas (the engine's proportion). It lives in its own Core Animation layers: each
/// frame only replaces two paths, so the rest of the window is never re-rendered, and frames are
/// requested only while the line moves, the window is on screen and the app is in front.
struct LineaViva: View {
    var pose: LineaMotor.Pose = .rest
    var etiqueta = "Fogar"
    /// Never animate (menu bar, sidebar mark).
    var quieta = false

    @Environment(\.accessibilityReduceMotion) private var reduce

    var body: some View {
        LineaCapa(pose: pose, quieta: quieta || reduce)
            .aspectRatio(LineaGeometry.width / LineaGeometry.height, contentMode: .fit)
            .accessibilityElement()
            .accessibilityLabel(etiqueta)
            .accessibilityAddTraits(.isImage)
    }
}

private struct LineaCapa: NSViewRepresentable {
    let pose: LineaMotor.Pose
    let quieta: Bool

    func makeNSView(context: Context) -> Vista { Vista(quieta: quieta, pose: pose) }

    func updateNSView(_ v: Vista, context: Context) {
        v.quieta = quieta
        v.pedir(pose)
    }

    final class Vista: NSView {
        var quieta: Bool { didSet { if quieta != oldValue { redibujar(); reprogramar() } } }
        private let motor = LineaMotor()
        private let trazo = CAShapeLayer(), cintas = CAShapeLayer()
        private var link: CADisplayLink?
        private var despertar: Timer?, calma: Timer?
        private var pose: LineaMotor.Pose
        private var ultimoCambio = -10.0

        init(quieta: Bool, pose: LineaMotor.Pose) {
            self.quieta = quieta
            self.pose = pose
            super.init(frame: .zero)
            wantsLayer = true
            for l in [trazo, cintas] { layer?.addSublayer(l) }
            trazo.fillColor = nil
            trazo.lineCap = .round
            trazo.lineJoin = .round
            cintas.strokeColor = nil
            cintas.fillRule = .nonZero
            motor.go(pose, at: Self.ahora)
            let nc = NotificationCenter.default
            nc.addObserver(self, selector: #selector(reprogramar), name: NSApplication.didBecomeActiveNotification, object: nil)
            nc.addObserver(self, selector: #selector(reprogramar), name: NSApplication.didResignActiveNotification, object: nil)
        }

        required init?(coder: NSCoder) { nil }

        private static var ahora: Double { CACurrentMediaTime() }

        /// A new pose, at most once every 2 s: a flapping state must not make the line shiver.
        func pedir(_ nueva: LineaMotor.Pose) {
            guard nueva != pose else { return }
            pose = nueva
            calma?.invalidate()
            let espera = ultimoCambio + 2 - Self.ahora
            if espera > 0 && !quieta {
                let t = Timer(timeInterval: espera, repeats: false) { [weak self] _ in
                    MainActor.assumeIsolated { self?.aplicar() }
                }
                RunLoop.main.add(t, forMode: .common)
                calma = t
            } else {
                aplicar()
            }
        }

        private func aplicar() {
            ultimoCambio = Self.ahora
            motor.go(pose, at: Self.ahora)
            redibujar()
            reprogramar()
        }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            NotificationCenter.default.removeObserver(self, name: NSWindow.didChangeOcclusionStateNotification, object: nil)
            if let window {
                NotificationCenter.default.addObserver(self, selector: #selector(reprogramar),
                                                       name: NSWindow.didChangeOcclusionStateNotification, object: window)
            } else {
                calma?.invalidate()
            }
            reprogramar() // out of a window: stops the display link and the wake-up timer
        }

        override func layout() {
            super.layout()
            redibujar()
        }

        override func viewDidChangeEffectiveAppearance() {
            super.viewDidChangeEffectiveAppearance()
            redibujar()
        }

        /// Runs the display link only when there is something to show moving.
        @objc private func reprogramar() {
            despertar?.invalidate()
            despertar = nil
            let visible = window?.occlusionState.contains(.visible) ?? false
            guard !quieta, visible, NSApp.isActive else { parar(); return }
            if let hasta = motor.quietUntil(Self.ahora) {
                parar()
                guard hasta.isFinite else { return }
                let t = Timer(timeInterval: max(0.02, hasta - Self.ahora), repeats: false) { [weak self] _ in
                    MainActor.assumeIsolated { self?.arrancar() }
                }
                t.tolerance = 0.05
                RunLoop.main.add(t, forMode: .common)
                despertar = t
            } else {
                arrancar()
            }
        }

        private func arrancar() {
            guard link == nil else { return }
            let l = displayLink(target: self, selector: #selector(fotograma))
            l.add(to: .main, forMode: .common)
            link = l
        }

        private func parar() {
            link?.invalidate()
            link = nil
        }

        @objc private func fotograma() {
            redibujar()
            if motor.quietUntil(Self.ahora) != nil { reprogramar() }
        }

        private func redibujar() {
            guard bounds.width > 1 else { return }
            let (pts, mul): ([CGPoint], [Double]?) = quieta ? (motor.still(pose), nil) : motor.frame(at: Self.ahora)
            let s = bounds.width / LineaGeometry.width
            // Never thinner than 1 pt on screen: at sidebar size the brand's 4.4 would vanish.
            let w = max(LineaMotor.stroke, 1.0 / s)
            let (cuerpo, bordes) = LineaDibujo.caminos(pts, mul, w)
            var m = CGAffineTransform(scaleX: s, y: s)
            let tinta = NSColor(Color.hueso)
            var color = tinta.cgColor
            effectiveAppearance.performAsCurrentDrawingAppearance { color = tinta.cgColor }
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            for l in [trazo, cintas] { l.frame = bounds; l.contentsScale = window?.backingScaleFactor ?? 2 }
            trazo.path = cuerpo.copy(using: &m)
            trazo.lineWidth = w * s
            trazo.strokeColor = color
            cintas.path = bordes.copy(using: &m)
            cintas.fillColor = color
            CATransaction.commit()
        }

        override var isFlipped: Bool { true } // the engine's y grows downwards

    }
}

/// The engine's painting, as paths: one stroke for the body + ribbons only where the width changes
/// (the tapered ends, and the ember travelling the stroke during a wave).
enum LineaDibujo {
    static func caminos(_ pts: [CGPoint], _ mul: [Double]?, _ w: Double) -> (CGPath, CGPath) {
        let n = pts.count
        let tin = Int((Double(n) * 0.02).rounded()), tout = Int((Double(n) * 0.05).rounded()), tip = LineaMotor.tip / LineaMotor.stroke
        func taper(_ i: Int) -> Double {
            if i < tin { return tip + (1 - tip) * smooth(Double(i) / Double(tin)) }
            if i > n - 1 - tout { return 0.14 + 0.86 * smooth(Double(n - 1 - i) / Double(tout)) }
            return 1
        }
        let wf = { (i: Int) in w * taper(i) * (mul?[i] ?? 1) }

        let cuerpo = CGMutablePath()
        cuerpo.move(to: pts[tin])
        for i in (tin + 1)..<(n - 1 - tout) { cuerpo.addQuadCurve(to: mid(pts[i], pts[i + 1]), control: pts[i]) }
        cuerpo.addLine(to: pts[n - 1 - tout])

        let bordes = CGMutablePath()
        ribbon(bordes, pts, 0, tin, wf)
        ribbon(bordes, pts, n - 1 - tout, n - 1, wf)
        if let mul {
            var i = 0
            while i < n {
                while i < n && mul[i] <= 1.002 { i += 1 }
                let s0 = i
                while i < n && mul[i] > 1.002 { i += 1 }
                if i > s0 + 1 { ribbon(bordes, pts, max(0, s0 - 1), min(n - 1, i), wf) }
            }
        }
        return (cuerpo, bordes)
    }

    private static func smooth(_ b: Double) -> Double { b * b * b * (b * (b * 6 - 15) + 10) }
    private static func mid(_ a: CGPoint, _ b: CGPoint) -> CGPoint { CGPoint(x: (a.x + b.x) / 2, y: (a.y + b.y) / 2) }

    /// Variable-width outline, one winding direction so nonzero fill joins the crossings.
    private static func ribbon(_ path: CGMutablePath, _ pts: [CGPoint], _ a: Int, _ b: Int, _ wf: (Int) -> Double) {
        let n = pts.count
        var left: [CGPoint] = [], right: [CGPoint] = []
        var nx = 0.0, ny = -1.0
        for i in a...b {
            let p = pts[max(0, i - 1)], q = pts[min(n - 1, i + 1)]
            let dx = q.x - p.x, dy = q.y - p.y, l = hypot(dx, dy)
            if l > 1e-6 { nx = -dy / l; ny = dx / l }
            let h = wf(i) / 2
            left.append(CGPoint(x: pts[i].x + nx * h, y: pts[i].y + ny * h))
            right.append(CGPoint(x: pts[i].x - nx * h, y: pts[i].y - ny * h))
        }
        let e = left + right.reversed()
        path.move(to: e[0])
        for k in 1..<(e.count - 1) {
            if k == left.count - 1 || k == left.count { path.addLine(to: e[k]); continue }
            path.addQuadCurve(to: mid(e[k], e[k + 1]), control: e[k])
        }
        path.addLine(to: e[e.count - 1])
        path.closeSubpath()
        for i in [a, b] {
            let r = wf(i) / 2
            path.addEllipse(in: CGRect(x: pts[i].x - r, y: pts[i].y - r, width: 2 * r, height: 2 * r))
        }
    }
}
