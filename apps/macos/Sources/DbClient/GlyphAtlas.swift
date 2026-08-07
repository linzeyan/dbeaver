import CoreText
import Metal
import Foundation
import AppKit

/// Monospaced ASCII glyphs rasterized once into a single texture.
///
/// A grid draws the same ~95 shapes tens of thousands of times per frame.
/// Rasterizing them once and drawing textured quads is what keeps text off the
/// per-frame cost curve — this is the difference between a grid that scrolls at
/// refresh rate and one that does not.
final class GlyphAtlas {
    static let firstChar: UInt8 = 32
    static let lastChar: UInt8 = 126
    static let count = Int(lastChar - firstChar) + 1

    let texture: MTLTexture
    let cellWidth: Float
    let cellHeight: Float
    /// Advance width in points — the layout unit for the whole grid.
    let advance: Float
    let ascent: Float

    private let atlasColumns: Int

    init?(device: MTLDevice, pointSize: CGFloat, scale: CGFloat) {
        let font = CTFontCreateWithName("SF Mono" as CFString, pointSize, nil)
        let ascentPt = CTFontGetAscent(font)
        let descentPt = CTFontGetDescent(font)

        // Measure the advance of a representative glyph; the font is monospaced
        // so one sample defines the grid.
        var glyph = CTFontGetGlyphWithName(font, "zero" as CFString)
        var advanceSize = CGSize.zero
        CTFontGetAdvancesForGlyphs(font, .horizontal, &glyph, &advanceSize, 1)

        let cw = ceil(advanceSize.width * scale)
        let ch = ceil((ascentPt + descentPt) * scale)
        self.cellWidth = Float(cw)
        self.cellHeight = Float(ch)
        self.advance = Float(advanceSize.width)
        self.ascent = Float(ascentPt)

        // Lay the glyphs out in a square-ish grid to keep the texture compact.
        let cols = 16
        self.atlasColumns = cols
        let rows = (Self.count + cols - 1) / cols
        let texW = Int(cw) * cols
        let texH = Int(ch) * rows

        guard let ctx = CGContext(
            data: nil, width: texW, height: texH,
            bitsPerComponent: 8, bytesPerRow: texW,
            space: CGColorSpaceCreateDeviceGray(),
            bitmapInfo: CGImageAlphaInfo.none.rawValue
        ) else { return nil }

        ctx.setFillColor(CGColor(gray: 0, alpha: 1))
        ctx.fill(CGRect(x: 0, y: 0, width: texW, height: texH))
        ctx.setFillColor(CGColor(gray: 1, alpha: 1))
        ctx.setAllowsAntialiasing(true)
        ctx.setShouldAntialias(true)
        ctx.setShouldSmoothFonts(false)

        for i in 0..<Self.count {
            let ch8 = Self.firstChar + UInt8(i)
            guard ch8 != 32 else { continue }  // space rasterizes to nothing
            let col = i % cols
            let row = i / cols
            // CGContext origin is bottom-left; place each cell accordingly.
            let originX = CGFloat(col) * cw
            let originY = CGFloat(texH) - CGFloat(row + 1) * ch

            var char = UniChar(ch8)
            var g = CGGlyph()
            guard CTFontGetGlyphsForCharacters(font, &char, &g, 1) else { continue }
            var pos = CGPoint(x: originX, y: originY + descentPt * scale)
            ctx.saveGState()
            ctx.scaleBy(x: scale, y: scale)
            // Undo the scale for the position we already computed in pixels.
            pos.x /= scale
            pos.y /= scale
            CTFontDrawGlyphs(font, &g, &pos, 1, ctx)
            ctx.restoreGState()
        }

        let desc = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: .r8Unorm, width: texW, height: texH, mipmapped: false)
        desc.usage = .shaderRead
        guard let tex = device.makeTexture(descriptor: desc), let data = ctx.data else {
            return nil
        }
        tex.replace(
            region: MTLRegionMake2D(0, 0, texW, texH),
            mipmapLevel: 0,
            withBytes: data,
            bytesPerRow: texW)
        self.texture = tex
    }

    /// Normalized atlas rect for an ASCII byte, or nil if out of range.
    func uv(for byte: UInt8) -> (x: Float, y: Float, w: Float, h: Float)? {
        guard byte >= Self.firstChar, byte <= Self.lastChar else { return nil }
        let i = Int(byte - Self.firstChar)
        let col = i % atlasColumns
        let row = i / atlasColumns
        let texW = Float(texture.width), texH = Float(texture.height)
        return (
            x: Float(col) * cellWidth / texW,
            y: Float(row) * cellHeight / texH,
            w: cellWidth / texW,
            h: cellHeight / texH
        )
    }
}
