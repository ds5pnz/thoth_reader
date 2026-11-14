// Draws the Thoth Reader app icon: "Th" on the source page, "th" on the translated page, and
// the "o" between them as the pivot the two sides share.  usage: swift scripts/make-icon.swift out.png
import AppKit

let size: CGFloat = 1024
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size), bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
let ctx = NSGraphicsContext.current!.cgContext

func rgb(_ hex: Int, _ a: CGFloat = 1) -> NSColor {
  NSColor(srgbRed: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255, blue: CGFloat(hex & 0xff) / 255, alpha: a)
}

// macOS icon grid: an 824pt rounded square centred on the 1024 canvas, with a soft shadow.
let plate = CGRect(x: 100, y: 100, width: 824, height: 824)
let platePath = CGPath(roundedRect: plate, cornerWidth: 186, cornerHeight: 186, transform: nil)
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -12), blur: 28, color: rgb(0x000000, 0.32).cgColor)
ctx.addPath(platePath)
ctx.setFillColor(rgb(0x1a4f59).cgColor)
ctx.fillPath()
ctx.restoreGState()
ctx.saveGState()
ctx.addPath(platePath)
ctx.clip()
let bg = CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(), colors: [rgb(0x2a7482).cgColor, rgb(0x123a43).cgColor] as CFArray, locations: [0, 1])!
ctx.drawLinearGradient(bg, start: CGPoint(x: 512, y: plate.maxY), end: CGPoint(x: 512, y: plate.minY), options: [])
ctx.restoreGState()

// Two facing pages.
let pageW: CGFloat = 330, pageH: CGFloat = 468, gutter: CGFloat = 88
let pageY = (size - pageH) / 2 - 6
let left = CGRect(x: 512 - gutter / 2 - pageW, y: pageY, width: pageW, height: pageH)
let right = CGRect(x: 512 + gutter / 2, y: pageY, width: pageW, height: pageH)
func page(_ r: CGRect, _ fill: NSColor) {
  ctx.saveGState()
  ctx.setShadow(offset: CGSize(width: 0, height: -8), blur: 22, color: rgb(0x000000, 0.30).cgColor)
  ctx.addPath(CGPath(roundedRect: r, cornerWidth: 30, cornerHeight: 30, transform: nil))
  ctx.setFillColor(fill.cgColor)
  ctx.fillPath()
  ctx.restoreGState()
}
page(left, rgb(0xfbf8f0))
page(right, rgb(0xe4f1f2))

// Lettering. Both words sit on one baseline so the whole reads Th·o·th.
func font(_ design: NSFontDescriptor.SystemDesign, _ pt: CGFloat, _ weight: NSFont.Weight) -> NSFont {
  let base = NSFont.systemFont(ofSize: pt, weight: weight)
  if let d = base.fontDescriptor.withDesign(design), let f = NSFont(descriptor: d, size: pt) { return f }
  return base
}
// One size for both words: the largest at which the wider one still has margins on its page.
func width(_ s: String, _ f: NSFont) -> CGFloat {
  CTLineGetBoundsWithOptions(CTLineCreateWithAttributedString(NSAttributedString(string: s, attributes: [.font: f, .kern: -4])), .useGlyphPathBounds).width
}
var pt: CGFloat = 240
while pt > 80 && max(width("Th", font(.serif, pt, .bold)), width("th", font(.rounded, pt, .bold))) > pageW - 84 { pt -= 2 }
let serif = font(.serif, pt, .bold)
let sans = font(.rounded, pt, .bold)
func draw(_ s: String, _ f: NSFont, _ color: NSColor, in r: CGRect, baseline: CGFloat) {
  let a = NSAttributedString(string: s, attributes: [.font: f, .foregroundColor: color, .kern: -4])
  let line = CTLineCreateWithAttributedString(a)
  let b = CTLineGetBoundsWithOptions(line, .useGlyphPathBounds)
  ctx.textPosition = CGPoint(x: r.midX - b.width / 2 - b.minX, y: baseline)
  CTLineDraw(line, ctx)
}
let baseline = pageY + pageH / 2 - serif.capHeight / 2 + 4
draw("Th", serif, rgb(0x1d2126), in: left, baseline: baseline)
draw("th", sans, rgb(0x1f5f6b), in: right, baseline: baseline)

// The "o": a ring on the gutter at x-height, the hinge both pages turn on.
let xh = sans.xHeight
let d: CGFloat = max(xh * 1.12, gutter + 44)
let ring = CGRect(x: 512 - d / 2, y: baseline + xh / 2 - d / 2, width: d, height: d)
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -5), blur: 14, color: rgb(0x000000, 0.35).cgColor)
ctx.setFillColor(rgb(0xf4b63f).cgColor)
ctx.fillEllipse(in: ring)
ctx.restoreGState()
let hole = ring.insetBy(dx: d * 0.25, dy: d * 0.25)
ctx.setFillColor(rgb(0x18484f).cgColor)
ctx.fillEllipse(in: hole)

let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon.png"
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
