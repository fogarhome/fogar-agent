// Makes the macOS app icon from fogar-brand's square app icon (app/ios-1024.png, copied to
// Resources/AppIcon-1024.png): the 824 px rounded square of the macOS icon grid, centred on a
// transparent 1024 canvas, with the system's soft drop shadow. Output: Resources/AppIcon.png.
//   swift scripts/make-icon.swift
import AppKit

let dir = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
guard let src = NSImage(contentsOf: dir.appendingPathComponent("Resources/AppIcon-1024.png")),
      let cg = src.cgImage(forProposedRect: nil, context: nil, hints: nil),
      let ctx = CGContext(data: nil, width: 1024, height: 1024, bitsPerComponent: 8, bytesPerRow: 0,
                          space: CGColorSpace(name: CGColorSpace.sRGB) ?? CGColorSpaceCreateDeviceRGB(),
                          bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
else { fatalError("cannot read Resources/AppIcon-1024.png") }
let rect = CGRect(x: 100, y: 100, width: 824, height: 824)
let shape = CGPath(roundedRect: rect, cornerWidth: 185, cornerHeight: 185, transform: nil)
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -10), blur: 28, color: CGColor(gray: 0, alpha: 0.35))
ctx.addPath(shape)
ctx.setFillColor(CGColor(srgbRed: 11 / 255, green: 16 / 255, blue: 18 / 255, alpha: 1))
ctx.fillPath()
ctx.restoreGState()
ctx.addPath(shape)
ctx.clip()
ctx.draw(cg, in: rect)
// Hairline inner edge so the dark icon holds on a dark Dock.
ctx.resetClip()
ctx.addPath(CGPath(roundedRect: rect.insetBy(dx: 1, dy: 1), cornerWidth: 184, cornerHeight: 184, transform: nil))
ctx.setStrokeColor(CGColor(srgbRed: 236 / 255, green: 230 / 255, blue: 220 / 255, alpha: 0.12))
ctx.setLineWidth(2)
ctx.strokePath()
guard let out = ctx.makeImage(),
      let png = NSBitmapImageRep(cgImage: out).representation(using: .png, properties: [:])
else { fatalError("cannot encode") }
try png.write(to: dir.appendingPathComponent("Resources/AppIcon.png"))
print("wrote Resources/AppIcon.png")
