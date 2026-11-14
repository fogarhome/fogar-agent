import SwiftUI

/// The brand in the interface: the line that writes «Fogar» and changes shape with the state.
/// 1000 × 420 canvas (the engine's proportion). Frames are requested only while the line moves:
/// between waves, with the window hidden, or under reduce motion it costs nothing.
struct LineaViva: View {
    var pose: LineaMotor.Pose = .rest
    var etiqueta = "Fogar"
    /// Never animate (menu bar, sidebar mark).
    var quieta = false

    @Environment(\.accessibilityReduceMotion) private var reduce
    @Environment(\.ventanaVisible) private var visible
    @State private var motor = LineaMotor()
    @State private var aplicada: LineaMotor.Pose?
    @State private var calma: Task<Void, Never>?
    /// The engine has settled (between waves, or a still pose): the timeline stops asking for frames.
    @State private var quieto = false

    var body: some View {
        Group {
            if quieta || reduce {
                // Fixed poses with a 160 ms fade (fogar-brand «reducir movimiento»).
                Canvas { ctx, size in LineaViva.dibuja(ctx, size, motor.still(pose), nil) }
                    .id(pose)
                    .transition(.opacity)
                    .animation(.easeOut(duration: 0.16), value: pose)
            } else {
                TimelineView(.animation(minimumInterval: 1.0 / 60, paused: quieto || !visible)) { tl in
                    Canvas { ctx, size in
                        let f = motor.frame(at: tl.date.timeIntervalSinceReferenceDate)
                        LineaViva.dibuja(ctx, size, f.points, f.mul)
                    }
                }
            }
        }
        .aspectRatio(1000 / 420, contentMode: .fit)
        .accessibilityElement()
        .accessibilityLabel(etiqueta)
        .accessibilityAddTraits(.isImage)
        .onAppear {
            motor.go(pose, at: Date.timeIntervalSinceReferenceDate)
            aplicada = pose
        }
        .onChange(of: pose) { aplicar() }
        .task(id: aplicada) { await vigilar() }
    }

    /// Wakes the timeline only while the engine moves; sleeps until the next wave otherwise.
    private func vigilar() async {
        while !Task.isCancelled {
            let ahora = Date.timeIntervalSinceReferenceDate
            if let hasta = motor.quietUntil(ahora) {
                quieto = true
                guard hasta.isFinite else { return }
                try? await Task.sleep(for: .seconds(max(0.05, hasta - ahora)))
                quieto = false
                try? await Task.sleep(for: .milliseconds(300))
            } else {
                quieto = false
                try? await Task.sleep(for: .milliseconds(250))
            }
        }
    }

    /// The pose changes at most once every 2 s: a flapping state must not make the line shiver.
    private func aplicar() {
        calma?.cancel()
        calma = Task {
            try? await Task.sleep(for: .milliseconds(aplicada == nil ? 0 : 400))
            guard !Task.isCancelled else { return }
            motor.go(pose, at: Date.timeIntervalSinceReferenceDate)
            aplicada = pose
            try? await Task.sleep(for: .seconds(1.6))
        }
    }

    // MARK: Painting: one stroke pass + ribbons only where the width changes

    static func dibuja(_ ctx: GraphicsContext, _ size: CGSize, _ pts: [CGPoint], _ mul: [Double]?) {
        var ctx = ctx
        let s = size.width / LineaGeometry.width
        ctx.scaleBy(x: s, y: s)
        let shading = GraphicsContext.Shading.color(.hueso)
        // Never thinner than 1 pt on screen: at sidebar size the brand's 4.4 would vanish.
        let n = pts.count, w = max(LineaMotor.stroke, 1.0 / s)
        let tin = Int((Double(n) * 0.02).rounded()), tout = Int((Double(n) * 0.05).rounded()), tip = LineaMotor.tip / w
        func taper(_ i: Int) -> Double {
            if i < tin { return tip + (1 - tip) * smooth(Double(i) / Double(tin)) }
            if i > n - 1 - tout { return 0.14 + 0.86 * smooth(Double(n - 1 - i) / Double(tout)) }
            return 1
        }
        let wf = { (i: Int) in w * taper(i) * (mul?[i] ?? 1) }

        var path = Path()
        path.move(to: pts[tin])
        for i in (tin + 1)..<(n - 1 - tout) {
            path.addQuadCurve(to: mid(pts[i], pts[i + 1]), control: pts[i])
        }
        path.addLine(to: pts[n - 1 - tout])
        ctx.stroke(path, with: shading, style: StrokeStyle(lineWidth: w, lineCap: .round, lineJoin: .round))
        ribbon(&ctx, pts, 0, tin, wf, shading)
        ribbon(&ctx, pts, n - 1 - tout, n - 1, wf, shading)
        if let mul {
            var i = 0
            while i < n {
                while i < n && mul[i] <= 1.002 { i += 1 }
                let s0 = i
                while i < n && mul[i] > 1.002 { i += 1 }
                if i > s0 + 1 { ribbon(&ctx, pts, max(0, s0 - 1), min(n - 1, i), wf, shading) }
            }
        }
    }

    private static func smooth(_ b: Double) -> Double { b * b * b * (b * (b * 6 - 15) + 10) }
    private static func mid(_ a: CGPoint, _ b: CGPoint) -> CGPoint { CGPoint(x: (a.x + b.x) / 2, y: (a.y + b.y) / 2) }

    /// Variable-width outline, one winding direction so nonzero fill joins the crossings.
    private static func ribbon(_ ctx: inout GraphicsContext, _ pts: [CGPoint], _ a: Int, _ b: Int,
                               _ wf: (Int) -> Double, _ shading: GraphicsContext.Shading) {
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
        var path = Path()
        path.move(to: e[0])
        for k in 1..<(e.count - 1) {
            if k == left.count - 1 || k == left.count { path.addLine(to: e[k]); continue }
            path.addQuadCurve(to: mid(e[k], e[k + 1]), control: e[k])
        }
        path.addLine(to: e[e.count - 1])
        path.closeSubpath()
        ctx.fill(path, with: shading)
        for i in [a, b] {
            let r = wf(i) / 2
            ctx.fill(Path(ellipseIn: CGRect(x: pts[i].x - r, y: pts[i].y - r, width: 2 * r, height: 2 * r)), with: shading)
        }
    }
}
