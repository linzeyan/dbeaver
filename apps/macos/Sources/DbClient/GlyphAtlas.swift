import AppKit
import CoreText
import Foundation
import Metal

/// Monospaced ASCII glyphs rasterized once into a single texture.
///
/// A grid draws the same ~95 shapes tens of thousands of times per frame.
/// Rasterizing them once and drawing textured quads keeps text off the
/// per-frame cost curve — the difference between a grid that scrolls at refresh
/// rate and one that does not.
///
/// Everything here is computed in device pixels. Mixing points and pixels is
/// what produced the first version's misaligned text, so the font itself is
/// created at the scaled size and no conversion happens during rasterization.
final class GlyphAtlas {
    static let firstChar: UInt8 = 32
    static let lastChar: UInt8 = 126
    static let count = Int(lastChar - firstChar) + 1

    /// Glyphs the grid needs that ASCII does not have. Each gets a cell past the
    /// printable range, in this order, after the solid block.
    private static let extras: [UniChar] = [
        0x2026,  // … truncation marker
        0x25B2,  // ▲ sort ascending
        0x25BC  // ▼ sort descending
    ]

    /// Padding around each glyph in the atlas, in device pixels. Without it a
    /// glyph wider than its advance bleeds into the neighbouring cell and gets
    /// sampled as part of the wrong character.
    private static let pad: CGFloat = 2

    let texture: MTLTexture
    /// Atlas cell size in device pixels — also the size of each drawn quad.
    let cellWidth: Float
    let cellHeight: Float
    /// Horizontal advance in points: the grid's layout unit.
    let advance: Float
    /// Quad offset that compensates for the atlas padding, in device pixels.
    let quadInset: Float

    private let atlasColumns: Int

    init?(device: MTLDevice, pointSize: CGFloat, scale: CGFloat) {
        // CTFontCreateWithName silently substitutes a proportional face when the
        // named family is absent, and a proportional face corrupts every pen
        // position in the grid. monospacedSystemFont cannot fail that way.
        // Created at the scaled size so rasterization is pure device pixels.
        let font =
            NSFont.monospacedSystemFont(
                ofSize: pointSize * scale, weight: .regular) as CTFont

        let ascent = CTFontGetAscent(font)
        let descent = CTFontGetDescent(font)

        // Trust the measurement, not the API name: characters picked for maximum
        // width variation under a proportional font.
        let samples = Array("iWm0.".utf16)
        var sampleGlyphs = [CGGlyph](repeating: 0, count: samples.count)
        guard CTFontGetGlyphsForCharacters(font, samples, &sampleGlyphs, samples.count)
        else { return nil }
        var sampleAdvances = [CGSize](repeating: .zero, count: samples.count)
        CTFontGetAdvancesForGlyphs(
            font, .horizontal, sampleGlyphs, &sampleAdvances, samples.count)
        guard let advanceDevice = sampleAdvances.first?.width,
            advanceDevice > 0,
            sampleAdvances.allSatisfy({ abs($0.width - advanceDevice) < 0.01 })
        else {
            assertionFailure("font is not monospaced; grid layout would be wrong")
            return nil
        }

        let cw = ceil(advanceDevice) + Self.pad * 2
        let ch = ceil(ascent + descent) + Self.pad * 2
        self.cellWidth = Float(cw)
        self.cellHeight = Float(ch)
        self.advance = Float(advanceDevice / scale)
        self.quadInset = Float(Self.pad)

        // Past the printable range: first a solid block, so filled rectangles
        // (row banding, header background, selection) draw through the same
        // pipeline as text instead of needing a second one, then one cell per
        // entry in `extras`.
        let cellCount = Self.count + 1 + Self.extras.count
        let cols = 16
        self.atlasColumns = cols
        let rows = (cellCount + cols - 1) / cols
        let texW = Int(cw) * cols
        let texH = Int(ch) * rows

        guard
            let ctx = CGContext(
                data: nil, width: texW, height: texH,
                bitsPerComponent: 8, bytesPerRow: texW,
                space: CGColorSpaceCreateDeviceGray(),
                bitmapInfo: CGImageAlphaInfo.none.rawValue
            )
        else { return nil }

        ctx.setFillColor(CGColor(gray: 0, alpha: 1))
        ctx.fill(CGRect(x: 0, y: 0, width: texW, height: texH))
        ctx.setFillColor(CGColor(gray: 1, alpha: 1))
        ctx.setShouldAntialias(true)
        ctx.setShouldSmoothFonts(false)

        for i in 0..<Self.count {
            let byte = Self.firstChar + UInt8(i)
            guard byte != 32 else { continue }  // space has no shape

            var char = UniChar(byte)
            var glyph = CGGlyph()
            guard CTFontGetGlyphsForCharacters(font, &char, &glyph, 1) else { continue }

            let col = i % cols
            let row = i / cols
            // CGContext's origin is bottom-left; row 0 must land at the top of
            // the bitmap so it matches the UV mapping below.
            let cellLeft = CGFloat(col) * cw
            let cellBottom = CGFloat(texH) - CGFloat(row + 1) * ch
            var pos = CGPoint(x: cellLeft + Self.pad, y: cellBottom + Self.pad + descent)
            CTFontDrawGlyphs(font, &glyph, &pos, 1, ctx)
        }

        // Solid cell, immediately after the printable range.
        let solidCol = Self.count % cols
        let solidRow = Self.count / cols
        ctx.fill(
            CGRect(
                x: CGFloat(solidCol) * cw,
                y: CGFloat(texH) - CGFloat(solidRow + 1) * ch,
                width: cw, height: ch))

        // Extra glyphs, each centred in its cell: unlike the ASCII range these
        // are not necessarily the font's advance width, and drawing them at the
        // left edge would leave them visibly off-centre next to monospaced text.
        for (offset, scalar) in Self.extras.enumerated() {
            var char = scalar
            var glyph = CGGlyph()
            guard CTFontGetGlyphsForCharacters(font, &char, &glyph, 1), glyph != 0 else {
                assertionFailure("font lacks U+\(String(scalar, radix: 16)); grid marker missing")
                continue
            }
            var advance = CGSize.zero
            CTFontGetAdvancesForGlyphs(font, .horizontal, &glyph, &advance, 1)

            let index = Self.count + 1 + offset
            let col = index % cols
            let row = index / cols
            var pos = CGPoint(
                x: CGFloat(col) * cw + Self.pad + (advanceDevice - advance.width) / 2,
                y: CGFloat(texH) - CGFloat(row + 1) * ch + Self.pad + descent)
            CTFontDrawGlyphs(font, &glyph, &pos, 1, ctx)
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

    /// A single opaque texel, for drawing filled rectangles.
    ///
    /// Zero size means every corner samples the same texel, so the quad is a
    /// flat fill regardless of its dimensions.
    var solidUV: (x: Float, y: Float, w: Float, h: Float) {
        let col = Self.count % atlasColumns
        let row = Self.count / atlasColumns
        let texW = Float(texture.width)
        let texH = Float(texture.height)
        return (
            x: (Float(col) + 0.5) * cellWidth / texW,
            y: (Float(row) + 0.5) * cellHeight / texH,
            w: 0, h: 0
        )
    }

    /// The truncation marker.
    var ellipsisUV: (x: Float, y: Float, w: Float, h: Float) { extraUV(0) }
    var sortAscendingUV: (x: Float, y: Float, w: Float, h: Float) { extraUV(1) }
    var sortDescendingUV: (x: Float, y: Float, w: Float, h: Float) { extraUV(2) }

    private func extraUV(_ offset: Int) -> (x: Float, y: Float, w: Float, h: Float) {
        cellUV(at: Self.count + 1 + offset)
    }

    /// Normalized atlas rect for an ASCII byte, or nil if out of range.
    func uv(for byte: UInt8) -> (x: Float, y: Float, w: Float, h: Float)? {
        guard byte >= Self.firstChar, byte <= Self.lastChar else { return nil }
        return cellUV(at: Int(byte - Self.firstChar))
    }

    private func cellUV(at index: Int) -> (x: Float, y: Float, w: Float, h: Float) {
        let col = index % atlasColumns
        let row = index / atlasColumns
        let texW = Float(texture.width)
        let texH = Float(texture.height)
        return (
            x: Float(col) * cellWidth / texW,
            y: Float(row) * cellHeight / texH,
            w: cellWidth / texW,
            h: cellHeight / texH
        )
    }
}
