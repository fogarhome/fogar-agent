import CoreGraphics
import Foundation

/// The geometry of the living line: the one-stroke «Fogar» of fogar-brand's reference engine
/// (`linea-viva/index.html`, as copied into the gateway dashboard's `lineaViva.js`), ported
/// literally: same Sacramento F, same «ogar» curves, same sampling by arc length, same 600 points
/// fitted into a 1000 × 420 box. If the letter changes in fogar-brand, port `build()` again.
nonisolated struct LineaGeometry: Sendable {
    static let n = 600, width = 1000.0, height = 420.0
    static let shared = LineaGeometry()

    let word: [CGPoint]
    /// Index ranges of the named pieces along the 600 points (`IDX` in the engine).
    let idx: [String: (Int, Int)]
    let base: Double, xMin: Double, xMax: Double

    private init() {
        let (pts, idx) = Self.build(Self.n)
        let bb = Self.box(pts)
        let sc = min(800 / (bb.maxX - bb.minX), 372 / (bb.maxY - bb.minY))
        let fit = { (p: CGPoint) in
            CGPoint(x: 500 + (p.x - bb.midX) * sc, y: 214 + (p.y - bb.midY) * sc)
        }
        word = pts.map(fit)
        base = fit(CGPoint(x: 0, y: 250)).y
        let wb = Self.box(word)
        xMin = wb.minX
        xMax = wb.maxX
        self.idx = idx
    }

    // MARK: The letter (constants and paths exactly as in the engine)

    private static let sl = tan(7 * Double.pi / 180)
    /// Bowls of g and a 8 % narrower, rounded to 2 decimals like the engine's `toFixed(2)`.
    private static func nx(_ c: Double, _ x: Double) -> Double { ((c + (x - c) * 0.92) * 100).rounded() / 100 }
    private static func g(_ x: Double) -> Double { nx(272.5, x) }
    private static func a(_ x: Double) -> Double { nx(349, x) }
    private static let kF = 53.0 / 122
    private static func ftx(_ x: Double) -> Double { kF * x + 165 - 618 * kF }
    private static func fty(_ y: Double) -> Double { kF * y + 250 - 470 * kF }

    private static let fMain = "M 105 268 C 80 250 62 225 66 195 C 72 150 120 122 200 120 C 290 118 400 160 480 175 C 560 190 640 192 680 160 C 700 140 690 108 650 104 C 590 98 540 140 525 190 C 512 235 516 300 510 350 C 500 430 440 472 345 474 C 260 475 190 430 168 368"
    private static let fBar = "M 372 299 C 450 292 540 288 618 291"

    private static var ogar: [(String, String?)] {
        let ga = "C \(g(254)) 206 \(g(262)) 197 272 197 C \(g(283)) 197 \(g(291)) 200 \(g(291)) 211 C \(g(291)) 218 \(g(291)) 225 \(g(291)) 232"
        let aa = "C \(a(330)) 206 \(a(338)) 197 350 197 C \(a(361)) 197 \(a(368)) 200 \(a(368)) 211 C \(a(368)) 218 \(a(368)) 225 \(a(368)) 232"
        return [
            ("link", nil),
            ("oBody", "C 209 197 217 207 217 222 C 217 238 208 250 196 250 C 184 250 177 240 177 222"),
            ("oTop2", "C 177 206 185 197 197 197"),
            ("og", "C 205 197 210 200 214 204 C 222 213 236 240 246 240 C \(g(251)) 240 \(g(254)) 233 \(g(254)) 222"),
            ("gTop1", ga),
            ("gBody", "C \(g(291)) 243 \(g(283)) 250 272 250 C \(g(262)) 250 \(g(254)) 240 \(g(254)) 222"),
            ("gTop2", ga),
            ("gDesc", "C \(g(291)) 270 \(g(291)) 320 289 342 C 288 356 284 366 276 366 C 266 366 262 356 263 344 C 265 318 282 282 302 262 C 318 248 \(a(330)) 240 \(a(330)) 224"),
            ("aTop1", aa),
            ("aBody", "C \(a(368)) 243 \(a(359)) 250 348 250 C \(a(338)) 250 \(a(330)) 240 \(a(330)) 224"),
            ("aTop2", aa),
            ("r", "C \(a(368)) 242 369 250 376 251 C 382 252 386 246 389 240 C 394 230 400 206 404 184 C 413 166 411 146 397 146 C 383 146 382 163 388 177 C 393 192 404 198 418 198 C 428 198 434 202 434 212 C 434 228 432 238 436 245"),
            ("tail", "C 441 253 455 254 466 245 C 475 237 484 218 491 200"),
        ]
    }

    private static func slant(_ p: CGPoint) -> CGPoint { CGPoint(x: p.x + sl * (250 - p.y), y: p.y) }

    // MARK: Sampling (SVG getTotalLength / getPointAtLength for M + C paths)

    /// Points every ~0.5 units of arc length, like `sampleD`.
    private static func sample(_ d: String, transformF: Bool) -> [CGPoint] {
        let curve = flatten(d)
        var cum = [0.0]
        for i in 1..<curve.count { cum.append(cum[i - 1] + hypot(curve[i].x - curve[i - 1].x, curve[i].y - curve[i - 1].y)) }
        let total = cum[cum.count - 1]
        let n = max(2, Int((total * (transformF ? kF : 1) / 0.5).rounded(.up)))
        var out: [CGPoint] = []
        var j = 0
        for i in 0...n {
            let s = total * Double(i) / Double(n)
            while j < curve.count - 2 && cum[j + 1] < s { j += 1 }
            let f = (s - cum[j]) / max(cum[j + 1] - cum[j], 1e-12)
            let q = CGPoint(x: curve[j].x + (curve[j + 1].x - curve[j].x) * f, y: curve[j].y + (curve[j + 1].y - curve[j].y) * f)
            out.append(transformF ? CGPoint(x: ftx(q.x), y: fty(q.y)) : q)
        }
        return out
    }

    /// Dense polyline of an "M x y C x1 y1 x2 y2 x y …" path (256 steps per cubic: error ≪ 0.01).
    private static func flatten(_ d: String) -> [CGPoint] {
        let nums = d.split(separator: " ").compactMap { Double($0) }
        var p0 = CGPoint(x: nums[0], y: nums[1])
        var out = [p0]
        var i = 2
        while i + 5 < nums.count {
            let c1 = CGPoint(x: nums[i], y: nums[i + 1]), c2 = CGPoint(x: nums[i + 2], y: nums[i + 3]), p3 = CGPoint(x: nums[i + 4], y: nums[i + 5])
            for k in 1...256 {
                let t = Double(k) / 256, u = 1 - t
                let a = u * u * u, b = 3 * u * u * t, c = 3 * u * t * t, e = t * t * t
                out.append(CGPoint(x: a * p0.x + b * c1.x + c * c2.x + e * p3.x, y: a * p0.y + b * c1.y + c * c2.y + e * p3.y))
            }
            p0 = p3
            i += 6
        }
        return out
    }

    /// One stroke: the F top to bottom, back up its own ink to the bar, along it, out to the «o».
    private static func build(_ n: Int) -> ([CGPoint], [String: (Int, Int)]) {
        let main = sample(fMain, transformF: true), bar = sample(fBar, transformF: true)
        var best = (Double.greatestFiniteMagnitude, 0, 0)
        for (k, p) in main.enumerated() where !(p.y < fty(250) || p.y > fty(340) || p.x < ftx(480)) {
            for (j, q) in bar.enumerated() {
                let d = hypot(q.x - p.x, q.y - p.y)
                if d < best.0 { best = (d, k, j) }
            }
        }
        let (kc, jc) = (best.1, best.2)
        var pieces: [(String, [CGPoint])] = [
            ("Fmain", main), ("Fback", Array(main[kc...].reversed())),
            ("barL", Array(bar[...jc].reversed())), ("bar", bar),
        ]
        let yc = fty(291)
        var last = CGPoint(x: 165 - sl * (250 - yc), y: yc)
        for (k, seg) in ogar {
            let d = seg.map { "M \(last.x) \(last.y) \($0)" } ?? "M \(last.x) \(last.y) C 174 \(yc) 186 197 197 197"
            let pts = sample(d, transformF: false)
            last = pts[pts.count - 1]
            pieces.append((k, pts.map(slant)))
        }
        var all: [CGPoint] = [], cum = [0.0], mark: [String: (Double, Double)] = [:]
        for (k, pts) in pieces {
            let start = cum[cum.count - 1]
            for (i, q) in pts.enumerated() {
                if !all.isEmpty && i == 0 { continue }
                if let prev = all.last { cum.append(cum[cum.count - 1] + hypot(q.x - prev.x, q.y - prev.y)) }
                all.append(q)
            }
            mark[k] = (start, cum[cum.count - 1])
        }
        let total = cum[cum.count - 1]
        var out: [CGPoint] = []
        var j = 0
        for i in 0..<n {
            let d = total * Double(i) / Double(n - 1)
            while j < all.count - 2 && cum[j + 1] < d { j += 1 }
            let f = (d - cum[j]) / max(cum[j + 1] - cum[j], 1)
            out.append(CGPoint(x: all[j].x + (all[j + 1].x - all[j].x) * f, y: all[j].y + (all[j + 1].y - all[j].y) * f))
        }
        let idx = mark.mapValues { (Int(($0.0 / total * Double(n - 1)).rounded()), Int(($0.1 / total * Double(n - 1)).rounded())) }
        return (out, idx)
    }

    private static func box(_ pts: [CGPoint]) -> CGRect {
        let xs = pts.map(\.x), ys = pts.map(\.y)
        return CGRect(x: xs.min() ?? 0, y: ys.min() ?? 0, width: (xs.max() ?? 0) - (xs.min() ?? 0), height: (ys.max() ?? 0) - (ys.min() ?? 0))
    }
}
