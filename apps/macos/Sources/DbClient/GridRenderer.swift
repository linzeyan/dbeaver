import Metal
import MetalKit
import simd

/// Draws the visible window of the result grid as textured glyph quads.
///
/// Cost per frame is proportional to *visible* cells, not to result size — a
/// 1M-row result and a 1k-row result draw the same number of quads. If frame
/// time tracks result size, something is wrong upstream of the renderer.
final class GridRenderer {
    struct Uniforms {
        var viewport: SIMD2<Float>
    }

    struct GlyphInstance {
        var pos: SIMD2<Float>
        var size: SIMD2<Float>
        var uvOrigin: SIMD2<Float>
        var uvSize: SIMD2<Float>
    }

    // Layout, in points.
    let rowHeight: Float = 20
    let headerHeight: Float = 24
    let columnWidth: Float = 130
    let cellPadding: Float = 6
    /// Cells are clipped to this many characters; a grid cell shows a preview,
    /// not the full value.
    let maxCellChars = 18

    private let device: MTLDevice
    private let queue: MTLCommandQueue
    private let pipeline: MTLRenderPipelineState
    private let atlas: GlyphAtlas
    private let scale: Float

    /// Triple-buffered instance storage so the CPU never waits on the GPU.
    private var instanceBuffers: [MTLBuffer] = []
    private var frameIndex = 0
    private let maxInstances = 200_000
    private let inFlight = DispatchSemaphore(value: 3)

    private var instances: [GlyphInstance] = []

    var table: ArrowTable?
    var scrollRow: Double = 0

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

    func draw(in view: MTKView) {
        guard let table,
              let drawable = view.currentDrawable,
              let passDesc = view.currentRenderPassDescriptor,
              let cmd = queue.makeCommandBuffer()
        else { return }

        let start = CFAbsoluteTimeGetCurrent()
        inFlight.wait()

        let sizePts = view.bounds.size
        buildInstances(viewSize: sizePts, table: table)

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

        let rows = visibleRowRange(viewHeight: viewSize.height, rowCount: table.rowCount)
        let colCount = min(table.columns.count,
                           Int(ceil(Float(viewSize.width) / columnWidth)))

        // Header.
        for c in 0..<colCount {
            emit(text: table.columns[c].name,
                 x: Float(c) * columnWidth + cellPadding,
                 y: 4)
        }

        // Fractional scroll offset keeps motion smooth between whole rows.
        let subRow = Float(scrollRow - scrollRow.rounded(.down))
        for (i, r) in rows.enumerated() {
            let y = headerHeight + Float(i) * rowHeight - subRow * rowHeight
            guard y < Float(viewSize.height) else { break }
            for c in 0..<colCount {
                let s = table.text(row: r, column: c)
                emit(text: s, x: Float(c) * columnWidth + cellPadding, y: y + 3)
            }
        }
    }

    private func emit(text: String, x: Float, y: Float) {
        var penX = x
        var n = 0
        for byte in text.utf8 {
            if n >= maxCellChars { break }
            n += 1
            guard let uv = atlas.uv(for: byte) else { penX += atlas.advance; continue }
            if byte != 32 {
                instances.append(GlyphInstance(
                    pos: SIMD2(penX * scale, y * scale),
                    size: SIMD2(atlas.cellWidth, atlas.cellHeight),
                    uvOrigin: SIMD2(uv.x, uv.y),
                    uvSize: SIMD2(uv.w, uv.h)))
            }
            penX += atlas.advance
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
    };

    struct VOut {
        float4 position [[position]];
        float2 uv;
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
        return o;
    }

    fragment float4 grid_fragment(VOut in [[stage_in]],
                                  texture2d<float> atlas [[texture(0)]]) {
        constexpr sampler s(filter::linear, address::clamp_to_edge);
        float a = atlas.sample(s, in.uv).r;
        return float4(0.88, 0.90, 0.93, a);
    }
    """
}
