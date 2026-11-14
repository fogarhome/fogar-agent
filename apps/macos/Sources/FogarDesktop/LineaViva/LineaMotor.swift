import CoreGraphics
import Foundation

/// The living line's engine for the poses this app uses (fogar-brand `linea-viva`, as in the
/// dashboard's `lineaViva.js`): `line` (the straight line), `rest` (the word, with the wave every
/// ~7 s, the tail's whip, and every third time the name gathering into the F), and the transitions
/// between them (a wide front carrying the old shape to the new, with a travelling wave on it), all
/// through critically damped springs on 600 points. Listen/think/tool/speak arrive with voice (S3).
/// The line is always whole: it never draws itself progressively.
final class LineaMotor {
    enum Pose { case line, rest, fonly }

    private let geo = LineaGeometry.shared
    private let n = LineaGeometry.n
    private typealias V2 = SIMD2<Double>

    private var p: [V2], v: [V2], tn: [V2], told: [V2], tnew: [V2], snap: [V2], gs: [V2], gw: [V2]
    private var eb: [Double], wdy: [Double], wmul: [Double]
    private let word: [V2], xm: [Double], tailW: [Double], damp: [Double]
    private let lx0: Double, lx1: Double, base: Double, xF: Double, xT: Double, xBC: Double, xBT: Double
    private let iBar0: Int, iBarL0: Int, iFEnd: Int, ts: Int

    private(set) var cur: Pose = .line
    private var prevSnap = false, prev: Pose = .line
    private var tChange = -10.0, sweepDir = 1.0, fast = 0.65, idle = 0
    private var waveT0 = -99.0, nextWave = 1e9, whipT0 = -99.0, whipDone = false, seed: UInt64 = 7
    private var pending: (Pose, Double)?, pendingAt: Double?
    private var clock: Double?, acc = 0.0

    static let stroke = 4.4, tip = 0.8
    var tin: Int { Int((Double(n) * 0.02).rounded()) }
    var tout: Int { Int((Double(n) * 0.05).rounded()) }

    init() {
        word = geo.word.map { V2($0.x, $0.y) }
        let z = [V2](repeating: .zero, count: n)
        p = z; v = z; tn = z; told = z; tnew = z; snap = z; gs = z; gw = z
        eb = .init(repeating: 0, count: n); wdy = eb; wmul = .init(repeating: 1, count: n)
        lx0 = geo.xMin; lx1 = geo.xMax; base = geo.base
        iBar0 = geo.idx["bar"]?.0 ?? 0; iBarL0 = geo.idx["barL"]?.0 ?? 0
        iFEnd = geo.idx["bar"]?.1 ?? 0; ts = geo.idx["tail"]?.0 ?? n - 1
        var m = [Double](); for (i, q) in word.enumerated() { m.append(max(q.x, i > 0 ? m[i - 1] : -1e9)) }
        xm = m
        let (ts, n) = (ts, n)
        tailW = (0..<n).map { Self.smooth(Self.clamp(Double($0 - ts) / Double(n - 1 - ts))) }
        damp = (0..<n).map { 25.3 - (25.3 - 16) * Self.smooth(Self.clamp(Double($0 - ts) / (0.03 * Double(n - 1)))) }
        xF = geo.xMin; xT = word[ts].x; xBC = word[iBarL0].x; xBT = word[iFEnd].x
        for i in 0..<n { p[i] = V2(lx0 + (lx1 - lx0) * Double(i) / Double(n - 1), base) }
    }

    // MARK: Public

    /// Moves to `pose` at time `t` (seconds, any monotonic clock).
    func go(_ to: Pose, at t: Double) {
        guard to != cur else { return }
        if cur == .rest {
            waveFill(t - waveT0)
            for i in 0..<n { p[i].y += wdy[i] } // the wave in flight passes to the springs: no jump
        }
        waveT0 = -99; whipT0 = -99; pending = nil; pendingAt = nil
        if t - tChange < 2.0 * fast { snap = p; prevSnap = true } else { prevSnap = false; prev = cur }
        fast = to == .fonly ? 0.6 : 0.65
        if to == .rest && prev != .fonly { idle = 0 }
        cur = to; tChange = t
        sweepDir = to == .fonly ? -1 : 1 // towards the name it writes left to right; towards the F it gathers
        if to == .rest { nextWave = t + 4.5; whipT0 = t + 2.0 * fast }
    }

    /// Advances the springs to `t` in fixed 1/120 s steps. Returns the points to draw and the
    /// width multiplier (the ember travelling the stroke during a wave).
    func frame(at t: Double) -> (points: [CGPoint], mul: [Double]) {
        let last = clock ?? t
        acc += min(0.25, max(0, t - last))
        clock = t
        let t0 = t - acc
        var k = 0.0
        while acc >= 1.0 / 120 {
            step(t0 + k / 120, 1.0 / 120)
            acc -= 1.0 / 120
            k += 1
        }
        if cur == .rest { waveFill(t - waveT0) } else { wdy = .init(repeating: 0, count: n); wmul = .init(repeating: 1, count: n) }
        return ((0..<n).map { CGPoint(x: p[$0].x, y: p[$0].y + wdy[$0]) }, wmul)
    }

    /// When the engine next needs frames: `nil` = now (moving); a date = it is settled until then.
    func quietUntil(_ t: Double) -> Double? {
        let moving = (t - tChange) / fast < 2.6 || pending != nil
            || (cur == .rest && (t - waveT0 < 3.0 || t - whipT0 < 0.6))
            || v.contains { abs($0.x) + abs($0.y) > 0.05 }
        if moving { return nil }
        return cur == .rest ? nextWave : .infinity
    }

    /// A still pose (reduce motion, the menu bar): the shape itself, no springs.
    func still(_ pose: Pose) -> [CGPoint] {
        var out = [V2](repeating: .zero, count: n)
        shape(pose, &out)
        return out.map { CGPoint(x: $0.x, y: $0.y) }
    }

    // MARK: Engine (names follow the reference)

    private static func clamp(_ x: Double, _ a: Double = 0, _ b: Double = 1) -> Double { max(a, min(b, x)) }
    private static func smooth(_ b: Double) -> Double { b * b * b * (b * (b * 6 - 15) + 10) }
    private static func ss(_ b: Double) -> Double { b * b * (3 - 2 * b) }

    private func rnd() -> Double {
        seed = (seed * 16807) % 2_147_483_647
        return Double(seed) / 2_147_483_647
    }

    private func shape(_ st: Pose, _ out: inout [V2]) {
        switch st {
        case .line: for i in 0..<n { out[i] = V2(lx0 + (lx1 - lx0) * Double(i) / Double(n - 1), base) }
        case .rest: out = word
        case .fonly: // the name gathered into the bar: what is left rolls into it, from the tip to the stem
            for i in 0..<n {
                let jf = i <= iFEnd ? Double(i) : Double(iFEnd) - Double(iFEnd - iBar0) * Double(i - iFEnd) / Double(n - 1 - iFEnd)
                let j0 = Int(jf.rounded(.down)), j1 = min(n - 1, j0 + 1), f = jf - Double(j0)
                out[i] = word[j0] + (word[j1] - word[j0]) * f
            }
        }
    }

    private static func g55(_ d: Double) -> Double { exp(-d * d / (2 * 55 * 55)) }

    /// 0 nothing, 1 the F's bar plucked, 2 the wave across the letters. Fills `wdy` and `wmul`.
    @discardableResult
    private func waveFill(_ tw: Double) -> Int {
        wdy = .init(repeating: 0, count: n); wmul = .init(repeating: 1, count: n)
        guard tw >= 0 && tw <= 2.8 else { return 0 }
        let c: Double, amp: Double
        if tw < 1 { // a plucked string: the F's arm bends 10 px and is let go; the ember runs along it
            let e = Self.smooth(Self.clamp(tw / 0.18)) * Self.smooth(Self.clamp((1 - tw) / 0.55))
            c = Double(iBar0) + Double(iFEnd - iBar0) * Self.ss(tw); amp = e
            if iBarL0 <= iFEnd {
                for i in iBarL0...iFEnd {
                    let q = (word[i].x - xBC) / (xBT - xBC)
                    if q > 0 && q < 1 { wdy[i] = -10 * e * pow(sin(.pi * q), 2) }
                }
            }
        } else { // y(x) from the f to the r: gaussian crest (σ 55 px, 7 px) with a 35 % trough behind
            let u = (tw - 1) / 1.8, e = Self.smooth(Self.clamp(u / 0.14)) * Self.smooth(Self.clamp((1 - u) / 0.14))
            let xc = xF + (geo.xMax - xF) * u
            for i in 0..<n { wdy[i] = -7 * e * (Self.g55(word[i].x - xc) - 0.35 * Self.g55(word[i].x - xc + 1.8 * 55)) }
            var lo = 0, hi = n - 1
            while lo < hi { let m = (lo + hi) / 2; if xm[m] < xc { lo = m + 1 } else { hi = m } }
            c = Double(lo); amp = e
        }
        let sg = 0.03 * Double(n - 1)
        for i in max(0, Int((c - 3 * sg).rounded(.down)))...min(n - 1, Int((c + 3 * sg).rounded(.up))) {
            wmul[i] = 1 + 0.3 * amp * exp(-pow((Double(i) - c) / sg, 2) / 2)
        }
        return tw < 1 ? 1 : 2
    }

    private func target(_ t: Double, _ out: inout [V2]) {
        let since = (t - tChange) / fast
        if pending != nil && pendingAt == nil && since > 1.6 { pendingAt = t + (pending?.1 ?? 0) }
        shape(cur, &tnew)
        if prevSnap { told = snap } else { shape(prev, &told) }
        if since >= 2.0 { out = tnew } else {
            for i in 0..<n {
                let s = sweepDir > 0 ? Double(i) / Double(n - 1) : 1 - Double(i) / Double(n - 1)
                let e = Self.smooth(Self.clamp((since - 0.9 * s) / 1.1))
                eb[i] = e
                out[i] = told[i] + (tnew[i] - told[i]) * e
            }
            // The front is smoothed (no spikes where old and new cross) and carries the wave.
            gauss(out, &gw, 8)
            let a = cur == .rest ? 10.0 : cur == .fonly ? 8 : 16
            for i in 0..<n { let w = 4 * eb[i] * (1 - eb[i]); gs[i] = out[i] + (gw[i] - out[i]) * w }
            for i in 0..<n {
                let d0 = gs[min(n - 1, i + 3)] - gs[max(0, i - 3)]
                let l = max(hypot(d0.x, d0.y), 1e-9)
                let w = 4 * eb[i] * (1 - eb[i])
                let d = w * a * sin(.pi * 2 * 2.2 * Double(i) / Double(n - 1) - since * 5.5)
                out[i] = V2(gs[i].x - d0.y / l * d, gs[i].y + d0.x / l * d)
            }
            return
        }
        if cur == .rest {
            let tw = t - whipT0
            if tw >= 0 && tw < 0.3 { for i in ts..<n { out[i].y -= 9 * sin(.pi * tw / 0.3) * tailW[i] } }
        }
    }

    private func step(_ t: Double, _ dt: Double) {
        target(t, &tn)
        if let (st, _) = pending, let at = pendingAt, t >= at { pending = nil; pendingAt = nil; go(st, at: t) }
        if cur == .rest {
            if t >= nextWave {
                idle += 1
                if idle % 3 == 2 { go(.fonly, at: t); pending = (.rest, 1.2); return } // every third time: back into the F
                waveT0 = t; whipDone = false; nextWave = t + 7 + 3 * (rnd() - 0.5)
            }
            let tw = t - waveT0
            if !whipDone && tw > 1 && tw < 2.8 && xF + (geo.xMax - xF) * (tw - 1) / 1.8 >= xT { whipT0 = t; whipDone = true }
        }
        let soft = cur == .rest
        for i in 0..<n {
            let c = soft ? damp[i] : 25.3
            v[i] += (160 * (tn[i] - p[i]) - c * v[i]) * dt
        }
        for _ in 0..<2 { for i in 1..<(n - 1) { v[i] = 0.5 * v[i] + 0.25 * (v[i - 1] + v[i + 1]) } }
        for i in 0..<n { p[i] += v[i] * dt }
    }

    /// Gaussian smoothing along the arc (three box passes, ends reflected through the tip).
    private func gauss(_ src: [V2], _ out: inout [V2], _ sig: Double) {
        let r = max(1, Int(sqrt(sig * sig + 1).rounded()))
        var a = src
        var cs = [V2](repeating: .zero, count: n + 2 * r + 1)
        for _ in 0..<3 {
            var acc = V2.zero
            for j in 0..<(n + 2 * r) {
                let i = j - r
                let val: V2 = i < 0 ? 2 * a[0] - a[min(n - 1, -i)] : i >= n ? 2 * a[n - 1] - a[max(0, 2 * n - 2 - i)] : a[i]
                acc += val
                cs[j + 1] = acc
            }
            var b = a
            for i in 0..<n { b[i] = (cs[i + 2 * r + 1] - cs[i]) / Double(2 * r + 1) }
            a = b
        }
        out = a
    }
}
