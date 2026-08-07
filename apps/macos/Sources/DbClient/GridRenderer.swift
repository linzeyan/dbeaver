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

    // Colours come from `Theme.Grid`, not from constants here. The grid and the
    // SwiftUI chrome around it used to carry separate palettes, which is what
    // made them look like two applications sharing a window.

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
    /// The cell under the cursor keys. Drawn as a row band plus a stronger cell
    /// fill, because a database grid is navigated by row and read by cell.
    var selection: GridSelection?
    /// Whether the grid holds keyboard focus. Drawn as an inset border: the
    /// selection alone does not say which surface the arrow keys will move.
    var isFocused = false

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
        func rowY(_ i: Int) -> Float {
            headerHeight + Float(i) * rowHeight - subRow * rowHeight
        }

        // Row banding, drawn first so text lands on top.
        for (i, r) in rows.enumerated() where r % 2 == 1 {
            let y = rowY(i)
            guard y + rowHeight > headerHeight, y < viewH else { continue }
            fill(x: 0, y: y, w: viewW, h: rowHeight, color: Theme.Grid.banding.simd)
        }

        // Selection, between the banding and the separators so that the band
        // reads as continuous across the row and the grid lines stay on top.
        if let selection, rows.contains(selection.row) {
            let y = rowY(selection.row - rows.lowerBound)
            if y + rowHeight > headerHeight, y < viewH {
                fill(x: 0, y: y, w: viewW, h: rowHeight,
                     color: Theme.Grid.selectedRow.simd)
                let cx = Float(selection.column) * columnWidth - scrollX
                if cx + columnWidth > 0, cx < viewW {
                    fill(x: cx, y: y, w: columnWidth, h: rowHeight,
                         color: Theme.Grid.selectedCell.simd)
                    // A 1pt edge on the leading side marks the cell even where
                    // the fill sits over a dark value and washes out.
                    fill(x: cx, y: y, w: 1, h: rowHeight,
                         color: Theme.Grid.cursor.simd)
                }
            }
        }

        // Column separators.
        for c in firstCol..<lastCol {
            let x = Float(c) * columnWidth - scrollX
            guard x >= 0, x < viewW else { continue }
            fill(x: x, y: headerHeight, w: 1, h: viewH - headerHeight,
                 color: Theme.Grid.separator.simd)
        }

        // Header band, opaque so rows scroll beneath it.
        fill(x: 0, y: 0, w: viewW, h: headerHeight, color: Theme.Grid.header.simd)
        fill(x: 0, y: headerHeight - 1, w: viewW, h: 1,
             color: Theme.Grid.separator.simd)
        for c in firstCol..<lastCol {
            let x = Float(c) * columnWidth - scrollX + cellPadding
            emit(text: table.columns[c].name, x: x, y: 5,
                 color: Theme.Grid.headerText.simd)
        }

        // Cells.
        for (i, r) in rows.enumerated() {
            let y = rowY(i)
            guard y < viewH, y + rowHeight > headerHeight else { continue }
            for c in firstCol..<lastCol {
                let x = Float(c) * columnWidth - scrollX + cellPadding
                // NULL is drawn as the word, dimmed. `text` returns "" for both
                // NULL and an empty string, and rendering them the same way
                // hides a distinction the user is querying on.
                if table.isNull(row: r, column: c) {
                    emit(text: "NULL", x: x, y: y + 3, color: Theme.Grid.nullText.simd)
                } else {
                    emit(text: table.text(row: r, column: c), x: x, y: y + 3,
                         color: Theme.Grid.text.simd)
                }
            }
        }

        if isFocused {
            let ring = Theme.Grid.cursor.opacity(0.7).simd
            fill(x: 0, y: 0, w: viewW, h: 1, color: ring)
            fill(x: 0, y: viewH - 1, w: viewW, h: 1, color: ring)
            fill(x: 0, y: 0, w: 1, h: viewH, color: ring)
            fill(x: viewW - 1, y: 0, w: 1, h: viewH, color: ring)
        }
    }

    // MARK: - Hit testing

    /// The cell at a point in view coordinates, or `nil` for the header and the
    /// empty area past the last row or column.
    func cell(at point: CGPoint, viewHeight: CGFloat, table: ArrowTable) -> GridSelection? {
        let y = Float(point.y)
        guard y >= headerHeight else { return nil }
        let row = Int(scrollRow + Double((y - headerHeight) / rowHeight))
        let column = Int((Float(point.x) + scrollX) / columnWidth)
        guard row >= 0, row < table.rowCount,
              column >= 0, column < table.columns.count
        else { return nil }
        return GridSelection(row: row, column: column)
    }

    /// Scrolls the minimum distance that brings `selection` fully into view.
    ///
    /// Minimum rather than centred: keyboard navigation that recentres on every
    /// keystroke makes the surrounding rows impossible to track.
    func scrollToVisible(_ selection: GridSelection, viewSize: CGSize, columns: Int) {
        let visibleRows = Double(max(1, Int((Float(viewSize.height) - headerHeight) / rowHeight)))
        let row = Double(selection.row)
        if row < scrollRow {
            scrollRow = row
        } else if row >= scrollRow + visibleRows - 1 {
            scrollRow = row - visibleRows + 2
        }
        scrollRow = max(0, scrollRow)

        let left = Float(selection.column) * columnWidth
        if left < scrollX {
            scrollX = left
        } else if left + columnWidth > scrollX + Float(viewSize.width) {
            scrollX = left + columnWidth - Float(viewSize.width)
        }
        scrollX = max(0, min(scrollX, max(0, contentWidth(columns: columns) - Float(viewSize.width))))
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
