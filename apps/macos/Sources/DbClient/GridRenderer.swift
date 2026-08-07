import Metal
import MetalKit
import simd

/// Draws the visible window of the result grid as textured quads.
///
/// Cost per frame is proportional to *visible* cells, not to result size — a
/// 1M-row result and a 1k-row result draw the same number of quads. If frame
/// time tracks result size, something is wrong upstream of the renderer.
///
/// Text and filled rectangles share one pipeline: the glyph atlas reserves a
/// solid cell, so banding and backgrounds are just quads with a different UV.
final class GridRenderer {
    struct Uniforms {
        var viewport: SIMD2<Float>
    }

    struct GlyphInstance {
        var pos: SIMD2<Float>
        var size: SIMD2<Float>
        var uvOrigin: SIMD2<Float>
        var uvSize: SIMD2<Float>
        var color: SIMD4<Float>
    }

    private enum Palette {
        static let headerBackground = SIMD4<Float>(0.14, 0.15, 0.18, 1)
        static let headerText = SIMD4<Float>(0.62, 0.68, 0.78, 1)
        static let rowBanding = SIMD4<Float>(1, 1, 1, 0.022)
        static let separator = SIMD4<Float>(1, 1, 1, 0.06)
        static let text = SIMD4<Float>(0.88, 0.90, 0.93, 1)
        static let nullText = SIMD4<Float>(0.45, 0.48, 0.54, 1)
    }

    // Layout, in points.
    let rowHeight: Float = 20
    let headerHeight: Float = 24
    let columnWidth: Float = 130
    let cellPadding: Float = 6

    private let device: MTLDevice
    private let queue: MTLCommandQueue
    private let pipeline: MTLRenderPipelineState
    private let atlas: GlyphAtlas
    private let scale: Float
    /// Characters that fit in a cell. Derived from the column width rather than
    /// fixed, so text can never spill into the neighbouring column and be
    /// misread as that column's value.
    private let maxCellChars: Int

    /// Triple-buffered instance storage so the CPU never waits on the GPU.
    private var instanceBuffers: [MTLBuffer] = []
    private var frameIndex = 0
    private let maxInstances = 200_000
    private let inFlight = DispatchSemaphore(value: 3)

    private var instances: [GlyphInstance] = []

    var table: ArrowTable?
    var scrollRow: Double = 0
    /// Horizontal scroll offset in points.
    var scrollX: Float = 0

    /// Rolling frame-time stats, which are the measurement Phase 0 exists for.
    private(set) var lastFrameMs: Double = 0
    private(set) var frameSamples: [Double] = []

    init?(device: MTLDevice, scale: CGFloat) {
        self.device = device
        self.scale = Float(scale)
        guard let queue = device.makeCommandQueue(),
              let atlas = GlyphAtlas(device: device, pointSize: 12, scale: scale)
        else { return nil }
        self.queue = queue
        self.atlas = atlas
        self.maxCellChars = max(1, Int((columnWidth - cellPadding * 2) / atlas.advance))

        guard let library = try? device.makeLibrary(source: Self.shaderSource, options: nil),
              let vfn = library.makeFunction(name: "grid_vertex"),
              let ffn = library.makeFunction(name: "grid_fragment")
        else { return nil }

        let desc = MTLRenderPipelineDescriptor()
        desc.vertexFunction = vfn
        desc.fragmentFunction = ffn
        desc.colorAttachments[0].pixelFormat = .bgra8Unorm
        desc.colorAttachments[0].isBlendingEnabled = true
        desc.colorAttachments[0].sourceRGBBlendFactor = .sourceAlpha
        desc.colorAttachments[0].destinationRGBBlendFactor = .oneMinusSourceAlpha
        guard let pipeline = try? device.makeRenderPipelineState(descriptor: desc) else {
            return nil
        }
        self.pipeline = pipeline

        for _ in 0..<3 {
            guard let b = device.makeBuffer(
                length: MemoryLayout<GlyphInstance>.stride * maxInstances,
                options: .storageModeShared) else { return nil }
            instanceBuffers.append(b)
        }
        instances.reserveCapacity(maxInstances)
    }

    /// Total scrollable width in points, for clamping horizontal scroll.
    func contentWidth(columns: Int) -> Float {
        Float(columns) * columnWidth
    }

    func draw(in view: MTKView) {
        guard let table,
              let drawable = view.currentDrawable,
              let passDesc = view.currentRenderPassDescriptor,
              let cmd = queue.makeCommandBuffer()
        else { return }

        let start = CFAbsoluteTimeGetCurrent()
        inFlight.wait()

        buildInstances(viewSize: view.bounds.size, table: table)

        let buffer = instanceBuffers[frameIndex]
        frameIndex = (frameIndex + 1) % instanceBuffers.count
        let count = min(instances.count, maxInstances)
        instances.withUnsafeBytes { src in
            buffer.contents().copyMemory(
                from: src.baseAddress!,
                byteCount: count * MemoryLayout<GlyphInstance>.stride)
        }

        var uniforms = Uniforms(viewport: SIMD2(
            Float(view.drawableSize.width), Float(view.drawableSize.height)))

        guard let enc = cmd.makeRenderCommandEncoder(descriptor: passDesc) else {
            inFlight.signal()
            return
        }
        enc.setRenderPipelineState(pipeline)
        enc.setVertexBuffer(buffer, offset: 0, index: 0)
        enc.setVertexBytes(&uniforms, length: MemoryLayout<Uniforms>.stride, index: 1)
        enc.setFragmentTexture(atlas.texture, index: 0)
        if count > 0 {
            enc.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4,
                               instanceCount: count)
        }
        enc.endEncoding()

        cmd.addCompletedHandler { [weak self] _ in
            self?.inFlight.signal()
        }
        cmd.present(drawable)
        cmd.commit()

        lastFrameMs = (CFAbsoluteTimeGetCurrent() - start) * 1000
        frameSamples.append(lastFrameMs)
    }

    func resetStats() { frameSamples.removeAll(keepingCapacity: true) }

    /// Visible rows for the current scroll position and view height.
    func visibleRowRange(viewHeight: CGFloat, rowCount: Int) -> Range<Int> {
        let usable = Float(viewHeight) - headerHeight
        let first = max(0, Int(scrollRow))
        let visible = Int(ceil(usable / rowHeight)) + 1
        return first..<min(rowCount, first + visible)
    }

    private func buildInstances(viewSize: CGSize, table: ArrowTable) {
        instances.removeAll(keepingCapacity: true)

        let viewW = Float(viewSize.width)
        let viewH = Float(viewSize.height)
        let rows = visibleRowRange(viewHeight: viewSize.height, rowCount: table.rowCount)

        // Only the horizontal slice actually on screen; without this the whole
        // schema would be built every frame regardless of what is visible.
        let firstCol = max(0, Int(scrollX / columnWidth))
        let lastCol = min(
            table.columns.count,
            Int((scrollX + viewW) / columnWidth) + 1)
        guard firstCol < lastCol else { return }

        let subRow = Float(scrollRow - scrollRow.rounded(.down))

        // Row banding, drawn first so text lands on top.
        for (i, r) in rows.enumerated() where r % 2 == 1 {
            let y = headerHeight + Float(i) * rowHeight - subRow * rowHeight
            guard y + rowHeight > headerHeight, y < viewH else { continue }
            fill(x: 0, y: y, w: viewW, h: rowHeight, color: Palette.rowBanding)
        }

        // Column separators.
        for c in firstCol..<lastCol {
            let x = Float(c) * columnWidth - scrollX
            guard x >= 0, x < viewW else { continue }
            fill(x: x, y: headerHeight, w: 1, h: viewH - headerHeight,
                 color: Palette.separator)
        }

        // Header band, opaque so rows scroll beneath it.
        fill(x: 0, y: 0, w: viewW, h: headerHeight, color: Palette.headerBackground)
        for c in firstCol..<lastCol {
            let x = Float(c) * columnWidth - scrollX + cellPadding
            emit(text: table.columns[c].name, x: x, y: 5, color: Palette.headerText)
        }

        // Cells.
        for (i, r) in rows.enumerated() {
            let y = headerHeight + Float(i) * rowHeight - subRow * rowHeight
            guard y < viewH, y + rowHeight > headerHeight else { continue }
            for c in firstCol..<lastCol {
                let x = Float(c) * columnWidth - scrollX + cellPadding
                let s = table.text(row: r, column: c)
                emit(text: s, x: x, y: y + 3, color: Palette.text)
            }
        }
    }

    private func fill(x: Float, y: Float, w: Float, h: Float, color: SIMD4<Float>) {
        let uv = atlas.solidUV
        instances.append(GlyphInstance(
            pos: SIMD2((x * scale).rounded(), (y * scale).rounded()),
            size: SIMD2((w * scale).rounded(), (h * scale).rounded()),
            uvOrigin: SIMD2(uv.x, uv.y),
            uvSize: SIMD2(uv.w, uv.h),
            color: color))
    }

    /// Appends one quad per visible glyph.
    ///
    /// Positions are snapped to whole device pixels and derived from the
    /// character index rather than an accumulating pen, so rounding cannot drift
    /// across a long cell and sampling stays 1:1 with the atlas texels.
    ///
    /// Non-ASCII bytes are skipped but still consume a column. Phase 0 data is
    /// ASCII; real text handling arrives with the value formatters in phase 4.
    private func emit(text: String, x: Float, y: Float, color: SIMD4<Float>) {
        let py = (y * scale).rounded() - atlas.quadInset
        var n = 0
        for byte in text.utf8 {
            if n >= maxCellChars { break }
            let index = n
            n += 1
            guard byte != 32, let uv = atlas.uv(for: byte) else { continue }
            let px = ((x + Float(index) * atlas.advance) * scale).rounded() - atlas.quadInset
            instances.append(GlyphInstance(
                pos: SIMD2(px, py),
                size: SIMD2(atlas.cellWidth, atlas.cellHeight),
                uvOrigin: SIMD2(uv.x, uv.y),
                uvSize: SIMD2(uv.w, uv.h),
                color: color))
        }
    }

    private static let shaderSource = """
    #include <metal_stdlib>
    using namespace metal;

    struct Uniforms { float2 viewport; };

    struct GlyphInstance {
        float2 pos;
        float2 size;
        float2 uvOrigin;
        float2 uvSize;
        float4 color;
    };

    struct VOut {
        float4 position [[position]];
        float2 uv;
        float4 color;
    };

    vertex VOut grid_vertex(uint vid [[vertex_id]],
                            uint iid [[instance_id]],
                            const device GlyphInstance* inst [[buffer(0)]],
                            constant Uniforms& u [[buffer(1)]]) {
        float2 corner = float2(float(vid & 1), float(vid >> 1));
        GlyphInstance g = inst[iid];
        float2 px = g.pos + corner * g.size;
        float2 ndc = float2(px.x / u.viewport.x * 2.0 - 1.0,
                            1.0 - px.y / u.viewport.y * 2.0);
        VOut o;
        o.position = float4(ndc, 0.0, 1.0);
        o.uv = g.uvOrigin + corner * g.uvSize;
        o.color = g.color;
        return o;
    }

    fragment float4 grid_fragment(VOut in [[stage_in]],
                                  texture2d<float> atlas [[texture(0)]]) {
        // Nearest, not linear: quads are pixel-aligned and map 1:1 onto atlas
        // texels, so filtering would only blur and pull in neighbouring glyphs.
        constexpr sampler s(filter::nearest, address::clamp_to_edge);
        float a = atlas.sample(s, in.uv).r;
        return float4(in.color.rgb, in.color.a * a);
    }
    """
}
