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
    /// Two lines: the column name, and the type its values are in underneath.
    ///
    /// A type appended to the name would compete for width in a column already
    /// sized to its own content, and the loser would be the name — the thing the
    /// grid is navigated by. A second line spends eight points once per screen
    /// instead of spending them again on every column with a long type name.
    let headerHeight: Float = 32
    let cellPadding: Float = 6
    /// The two header lines, in the same coordinate `emitCell` takes for a row.
    private let headerNameY: Float = 2
    private let headerTypeY: Float = 15

    /// Bounds for an auto-sized column. The floor keeps a narrow column
    /// clickable; the ceiling stops one long text value from pushing every
    /// other column off screen.
    private let minColumnWidth: Float = 56
    private let maxColumnWidth: Float = 340
    /// How much of a column's width the type line may claim while auto-sizing.
    /// Thirteen characters is `numeric(12,2)`, the longest spelling that still
    /// reads as a type at a glance; past it the label truncates rather than the
    /// layout deforming around a column that is mostly its own header.
    private let maxTypeChars = 13
    /// Rows sampled when sizing columns. Enough to catch the common width
    /// without walking a million rows to lay out a screenful.
    private let widthSampleRows = 120

    private let device: MTLDevice
    private let queue: MTLCommandQueue
    private let pipeline: MTLRenderPipelineState
    private let atlas: GlyphAtlas
    private let scale: Float

    /// Per-column widths in points, sized to content when a result arrives and
    /// adjustable by dragging a header edge. A uniform width is wrong in both
    /// directions at once: it clips a timestamp while wasting half a screen on
    /// a boolean.
    private(set) var columnWidths: [Float] = []
    /// Prefix sums of `columnWidths`, so a column's x is a lookup rather than a
    /// running total recomputed per cell per frame.
    private var columnOffsets: [Float] = [0]

    /// Triple-buffered instance storage so the CPU never waits on the GPU.
    private var instanceBuffers: [MTLBuffer] = []
    private var frameIndex = 0
    private let maxInstances = 200_000
    private let inFlight = DispatchSemaphore(value: 3)

    private var instances: [GlyphInstance] = []

    var table: ArrowTable? {
        didSet { reconcileColumnLayout() }
    }
    /// PostgreSQL's declared type per column name, where the result came from a
    /// relation that has one. Empty otherwise, which is not an error state — see
    /// `rebuildTypeLabels`.
    ///
    /// Keyed by name because that is the only key the two sides share, and a
    /// browse is `SELECT *`: within one relation the names are unique, so the
    /// match is exact rather than positional-and-hopeful.
    var declaredTypes: [String: String] = [:] {
        didSet { if declaredTypes != oldValue { typeLabels.removeAll() } }
    }
    /// Columns not to draw, by index into `table.columns`.
    ///
    /// Given no width rather than removed from the layout, which is what keeps
    /// every index in this file — the selection, the sort, the pending marks,
    /// the hit test — an index into the result itself. A renderer that packed
    /// the drawn columns into their own numbering would need a map back at each
    /// of those, and one of them would be the one nobody remembered.
    ///
    /// Changing the set re-derives the widths, so a column coming back gets one.
    /// That costs whatever the user had dragged, which is a fair price for an
    /// act as deliberate as changing a setting.
    var hiddenColumns: Set<Int> = [] {
        didSet {
            guard hiddenColumns != oldValue else { return }
            columnWidths.removeAll()
            columnOffsets = [0]
        }
    }
    var scrollRow: Double = 0
    /// Horizontal scroll offset in points.
    var scrollX: Float = 0
    /// The cell under the cursor keys. Drawn as a row band plus a stronger cell
    /// fill, because a database grid is navigated by row and read by cell.
    var selection: GridSelection?
    /// Cells changed and not yet sent, marked so the change is visible from
    /// across the grid rather than only in the field that made it.
    var pending: Set<GridCell> = []
    /// Rows marked to be deleted when the changes are sent, tinted whole: the
    /// change is to the row rather than to anything in it, so there is no cell
    /// for the pending mark to sit on.
    var deleted: Set<Int> = []
    /// Rows added and not yet sent, drawn after the last one the database
    /// returned. They are not in `table` and cannot be: it is a window onto
    /// Arrow buffers the core owns, and these exist only here.
    var drafts: [DraftRow] = []
    /// Whether the grid holds keyboard focus. Drawn as an inset border: the
    /// selection alone does not say which surface the arrow keys will move.
    var isFocused = false
    /// Which column the result is ordered by, if any. Drawn in the header so the
    /// order the rows are in is visible from the rows themselves.
    var sort: GridSort?

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
            guard
                let b = device.makeBuffer(
                    length: MemoryLayout<GlyphInstance>.stride * maxInstances,
                    options: .storageModeShared)
            else { return nil }
            instanceBuffers.append(b)
        }
        instances.reserveCapacity(maxInstances)
    }

    // MARK: - Column geometry

    /// Total scrollable width in points, for clamping horizontal scroll.
    var contentWidth: Float { columnOffsets.last ?? 0 }

    func columnX(_ index: Int) -> Float {
        columnOffsets[min(index, columnOffsets.count - 1)]
    }

    func columnWidth(_ index: Int) -> Float {
        index < columnWidths.count ? columnWidths[index] : minColumnWidth
    }

    /// Column names the current widths were built for.
    private var layoutSignature: [String] = []

    /// One header type label per column, resolved when the result or the
    /// declared types change rather than per frame: the header is re-emitted
    /// every frame and a decimal's label allocates.
    private var typeLabels: [String] = []

    /// Drops the widths only when the columns themselves changed.
    ///
    /// Re-running a browse with a new filter replaces the table, but the columns
    /// are the same columns. Re-deriving widths from the new sample would undo a
    /// header drag every time the user hits Apply, which makes dragging feel
    /// broken rather than sticky. A different set of names is a different result
    /// and its old widths mean nothing, so those go.
    private func reconcileColumnLayout() {
        let signature = table?.columns.map(\.name) ?? []
        guard signature != layoutSignature else { return }
        layoutSignature = signature
        columnWidths.removeAll()
        columnOffsets = [0]
        typeLabels.removeAll()
    }

    /// Resolves what each column's header calls its type.
    ///
    /// The declared type wins wherever there is one. `numeric(12,2)` and
    /// `character varying(64)` say what the column was created as — which is
    /// what decides whether two values compare exactly — and the Arrow schema
    /// cannot tell `text` from `varchar`, or either from `jsonb`, at all. The
    /// Arrow kind is the fallback rather than a blank, because a computed column
    /// has no declaration to show and "what actually arrived" is still a true
    /// answer. Neither branch can state a type the column does not have, which
    /// is the only outcome worse than saying nothing.
    private func rebuildTypeLabels(for table: ArrowTable) {
        typeLabels = table.columns.map { column in
            declaredTypes[column.name].map(Self.shortened) ?? column.kind.label
        }
    }

    /// PostgreSQL's own short spelling for a declared type.
    ///
    /// `format_type` renders the SQL-standard names, and two of those differ
    /// only past the width a grid column has: `timestamp without time zone` and
    /// `timestamp with time zone` both truncate to `timestam…`, which is exactly
    /// the distinction a type label exists to draw. Every rewrite here is an
    /// alias the server itself accepts — `varchar(64)` and `character
    /// varying(64)` declare the same column — not an abbreviation invented here.
    private static func shortened(_ declared: String) -> String {
        // The length modifier can sit in the middle of the SQL spelling, as in
        // `timestamp(3) with time zone`, so the suffix is stripped and the base
        // word rewritten around whatever it left behind.
        for (suffix, alias) in [(" without time zone", ""), (" with time zone", "tz")]
        where declared.hasSuffix(suffix) {
            let head = String(declared.dropLast(suffix.count))
            guard !alias.isEmpty else { return head }
            guard let paren = head.firstIndex(of: "(") else { return head + alias }
            return String(head[..<paren]) + alias + String(head[paren...])
        }
        for (sql, alias) in [
            ("character varying", "varchar"), ("bit varying", "varbit"), ("character", "char")
        ] where declared.hasPrefix(sql) {
            return alias + declared.dropFirst(sql.count)
        }
        return declared
    }

    func setColumnWidth(_ width: Float, at index: Int) {
        // A hidden column has no edge to drag, and the clamp below would give it
        // the minimum width — putting an empty column back on screen through a
        // gesture aimed at its neighbour.
        guard index < columnWidths.count, !hiddenColumns.contains(index) else { return }
        columnWidths[index] = min(maxColumnWidth, max(minColumnWidth, width))
        rebuildColumnOffsets()
    }

    /// Sizes every column to the wider of its header and a sample of its values.
    ///
    /// Sampling rather than scanning: the width that matters is the one the
    /// visible rows need, and walking a million rows to lay out a screenful
    /// would cost more than every frame it saves.
    ///
    /// Reads `typeLabels`, so the caller rebuilds those first.
    private func layoutColumns(for table: ArrowTable) {
        let sample = min(table.rowCount, widthSampleRows)
        columnWidths = table.columns.indices.map { c in
            // Zero width is how a column is hidden. Everything downstream — the
            // offsets, the binary search that turns an x into a column, the
            // visible slice — then leaves it out without being told about it,
            // because a column occupying no points is one no point is inside.
            if hiddenColumns.contains(c) { return 0 }
            // The type gets a say in the width, but a bounded one. It has to be
            // recognisable, not complete, and a long type name is routinely
            // longer than every value it describes — letting it size the column
            // would push real data off screen to spell out a label.
            var chars = max(
                table.columns[c].name.utf8.count,
                min(typeLabels[c].utf8.count, maxTypeChars))
            for r in 0..<sample {
                // NULL renders as the word, so it sets a floor of four.
                let len =
                    table.isNull(row: r, column: c)
                    ? 4 : table.text(row: r, column: c).utf8.count
                chars = max(chars, len)
            }
            // One character of slack keeps the longest value off the separator.
            let width = cellPadding * 2 + Float(chars + 1) * atlas.advance
            return min(maxColumnWidth, max(minColumnWidth, width))
        }
        rebuildColumnOffsets()
    }

    /// Characters that fit in a column of `width`.
    private func charsFitting(_ width: Float) -> Int {
        max(1, Int((width - cellPadding * 2) / atlas.advance))
    }

    private func rebuildColumnOffsets() {
        columnOffsets = [0]
        columnOffsets.reserveCapacity(columnWidths.count + 1)
        var x: Float = 0
        for w in columnWidths {
            x += w
            columnOffsets.append(x)
        }
    }

    /// Index of the column containing `x` in content coordinates.
    func columnIndex(atX x: Float) -> Int? {
        guard x >= 0, x < contentWidth, !columnWidths.isEmpty else { return nil }
        var lo = 0
        var hi = columnWidths.count - 1
        while lo <= hi {
            let mid = (lo + hi) / 2
            if x < columnOffsets[mid] {
                hi = mid - 1
            } else if x >= columnOffsets[mid + 1] {
                lo = mid + 1
            } else {
                return mid
            }
        }
        return nil
    }

    /// The column boundary within `tolerance` points of `x`, as the index of the
    /// column to its left. Drives the header's resize handles.
    func columnEdge(nearX x: Float, tolerance: Float) -> Int? {
        // A hidden column's trailing edge sits exactly on its neighbour's, so
        // skipping it here is what makes the handle at that point belong to the
        // column the user can actually see.
        for c in columnWidths.indices
        where !hiddenColumns.contains(c) && abs(columnOffsets[c + 1] - x) <= tolerance {
            return c
        }
        return nil
    }

    /// The nearest drawn column to `from` in the direction `step` points, or
    /// `from` itself where there is none.
    ///
    /// The keyboard is the one way a cursor can reach a hidden column: a click
    /// cannot land on something zero points wide, and a hidden column would
    /// otherwise swallow one press of the arrow key and leave the cursor
    /// apparently stuck.
    func drawnColumn(from: Int, step: Int) -> Int {
        var next = from
        while next >= 0, next < columnWidths.count {
            if !hiddenColumns.contains(next) { return next }
            next += step
        }
        return from
    }

    private func visibleColumns(viewWidth: Float) -> Range<Int> {
        guard !columnWidths.isEmpty else { return 0..<0 }
        let first = columnIndex(atX: max(0, min(scrollX, contentWidth - 1))) ?? 0
        var last = first
        while last < columnWidths.count, columnOffsets[last] < scrollX + viewWidth {
            last += 1
        }
        return first..<last
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

        var uniforms = Uniforms(
            viewport: SIMD2(
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
            enc.drawPrimitives(
                type: .triangleStrip, vertexStart: 0, vertexCount: 4,
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

    // MARK: - Scrollbars

    enum ScrollAxis { case vertical, horizontal }

    /// Where a scrollbar's track and thumb sit along its own axis, in points.
    struct ScrollbarMetrics {
        let trackStart: Float
        let trackLength: Float
        let thumbStart: Float
        let thumbLength: Float
    }

    /// Hit width. Wider than the drawn thumb: a 5pt target is a 5pt target
    /// whether or not the paint is subtle.
    let scrollbarGutter: Float = 12
    private let scrollbarThumbThickness: Float = 5
    /// A thumb sized purely by proportion vanishes on a million rows. Below this
    /// it stops reporting extent honestly and starts being a grab handle, which
    /// is the trade every platform makes.
    private let minThumbLength: Float = 28

    /// Which axis is being dragged, so its thumb can be drawn as engaged.
    var activeScrollAxis: ScrollAxis?

    /// Rows that fit below the header and above the horizontal gutter.
    ///
    /// The gutter is subtracted because the bar is drawn over the data: without
    /// it, scrolling to the end would park the last row underneath the bar,
    /// where it cannot be read.
    private func visibleRowSpan(viewSize: CGSize) -> Float {
        let gutter = contentWidth > Float(viewSize.width) ? scrollbarGutter : 0
        return max(1, (Float(viewSize.height) - headerHeight - gutter) / rowHeight)
    }

    /// Largest `scrollRow` that still fills the view.
    ///
    /// Scrolling past this would leave blank space below the last row, and would
    /// make the scrollbar reach its end before the data does.
    func maxScrollRow(viewSize: CGSize) -> Double {
        return max(0, Double(totalRows) - Double(visibleRowSpan(viewSize: viewSize)))
    }

    /// Every row the grid draws: what the database sent, and the drafts under
    /// them. Scrolling, hit testing and the scrollbar all count in these, so a
    /// draft is reachable by every means a fetched row is.
    var totalRows: Int { (table?.rowCount ?? 0) + drafts.count }

    func maxScrollX(viewWidth: CGFloat) -> Float {
        max(0, contentWidth - Float(viewWidth))
    }

    /// Whether each bar is needed. Computed together because each one shortens
    /// the other's track.
    private func scrollbarsNeeded(viewSize: CGSize) -> (vertical: Bool, horizontal: Bool) {
        guard table != nil else { return (false, false) }
        let vertical = Float(totalRows) > visibleRowSpan(viewSize: viewSize)
        let horizontal = contentWidth > Float(viewSize.width)
        return (vertical, horizontal)
    }

    func scrollbar(_ axis: ScrollAxis, viewSize: CGSize) -> ScrollbarMetrics? {
        let needed = scrollbarsNeeded(viewSize: viewSize)
        let viewW = Float(viewSize.width)
        let viewH = Float(viewSize.height)

        switch axis {
        case .vertical:
            guard needed.vertical, table != nil else { return nil }
            let span = visibleRowSpan(viewSize: viewSize)
            let trackStart = headerHeight
            let trackLength = viewH - headerHeight - (needed.horizontal ? scrollbarGutter : 0)
            guard trackLength > 0 else { return nil }
            let thumbLength = min(
                trackLength,
                max(minThumbLength, trackLength * span / Float(totalRows)))
            let maxScroll = maxScrollRow(viewSize: viewSize)
            let progress = maxScroll > 0 ? Float(scrollRow / maxScroll) : 0
            return ScrollbarMetrics(
                trackStart: trackStart, trackLength: trackLength,
                thumbStart: trackStart + (trackLength - thumbLength) * clamp01(progress),
                thumbLength: thumbLength)

        case .horizontal:
            guard needed.horizontal else { return nil }
            let trackLength = viewW - (needed.vertical ? scrollbarGutter : 0)
            guard trackLength > 0 else { return nil }
            let thumbLength = min(
                trackLength, max(minThumbLength, trackLength * viewW / contentWidth))
            let maxScroll = maxScrollX(viewWidth: CGFloat(viewW))
            let progress = maxScroll > 0 ? scrollX / maxScroll : 0
            return ScrollbarMetrics(
                trackStart: 0, trackLength: trackLength,
                thumbStart: (trackLength - thumbLength) * clamp01(progress),
                thumbLength: thumbLength)
        }
    }

    /// The axis whose gutter contains `point`, if any.
    func scrollbarAxis(at point: CGPoint, viewSize: CGSize) -> ScrollAxis? {
        let x = Float(point.x)
        let y = Float(point.y)
        if scrollbar(.vertical, viewSize: viewSize) != nil,
            x >= Float(viewSize.width) - scrollbarGutter, y >= headerHeight
        {
            return .vertical
        }
        if scrollbar(.horizontal, viewSize: viewSize) != nil,
            y >= Float(viewSize.height) - scrollbarGutter
        {
            return .horizontal
        }
        return nil
    }

    /// Point along `axis` that a drag is currently at.
    func scrollbarCoordinate(_ axis: ScrollAxis, of point: CGPoint) -> Float {
        axis == .vertical ? Float(point.y) : Float(point.x)
    }

    /// Moves the scroll so the thumb's leading edge lands on `thumbStart`.
    func scrollTo(_ axis: ScrollAxis, thumbStart: Float, viewSize: CGSize) {
        guard let m = scrollbar(axis, viewSize: viewSize) else { return }
        let travel = m.trackLength - m.thumbLength
        let progress = travel > 0 ? clamp01((thumbStart - m.trackStart) / travel) : 0
        switch axis {
        case .vertical:
            scrollRow = Double(progress) * maxScrollRow(viewSize: viewSize)
        case .horizontal:
            scrollX = progress * maxScrollX(viewWidth: viewSize.width)
        }
    }

    private func clamp01(_ v: Float) -> Float { min(1, max(0, v)) }

    private func emitScrollbars(viewSize: CGSize) {
        let viewW = Float(viewSize.width)
        let viewH = Float(viewSize.height)
        let inset = (scrollbarGutter - scrollbarThumbThickness) / 2

        if let m = scrollbar(.vertical, viewSize: viewSize) {
            let x = viewW - scrollbarGutter
            fill(
                x: x, y: m.trackStart, w: scrollbarGutter, h: m.trackLength,
                color: Theme.Grid.scrollTrack.simd)
            fill(
                x: x + inset, y: m.thumbStart,
                w: scrollbarThumbThickness, h: m.thumbLength,
                color: thumbColor(for: .vertical))
        }

        if let m = scrollbar(.horizontal, viewSize: viewSize) {
            let y = viewH - scrollbarGutter
            fill(
                x: m.trackStart, y: y, w: m.trackLength, h: scrollbarGutter,
                color: Theme.Grid.scrollTrack.simd)
            fill(
                x: m.thumbStart, y: y + inset,
                w: m.thumbLength, h: scrollbarThumbThickness,
                color: thumbColor(for: .horizontal))
        }
    }

    private func thumbColor(for axis: ScrollAxis) -> SIMD4<Float> {
        activeScrollAxis == axis
            ? Theme.Grid.scrollThumbActive.simd : Theme.Grid.scrollThumb.simd
    }

    /// Visible rows for the current scroll position and view height.
    func visibleRowRange(viewHeight: CGFloat, rowCount: Int) -> Range<Int> {
        let usable = Float(viewHeight) - headerHeight
        let first = max(0, Int(scrollRow))
        let visible = Int(ceil(usable / rowHeight)) + 1
        return first..<min(rowCount, first + visible)
    }

    private func buildInstances(viewSize: CGSize, table: ArrowTable) {
        instances.removeAll(keepingCapacity: true)

        // Labels before widths: a column's width now depends on its type label.
        if typeLabels.count != table.columns.count {
            rebuildTypeLabels(for: table)
        }
        if columnWidths.count != table.columns.count {
            layoutColumns(for: table)
        }

        let viewW = Float(viewSize.width)
        let viewH = Float(viewSize.height)
        let rows = visibleRowRange(viewHeight: viewSize.height, rowCount: totalRows)

        // Only the horizontal slice actually on screen; without this the whole
        // schema would be built every frame regardless of what is visible.
        let cols = visibleColumns(viewWidth: viewW)
        guard !cols.isEmpty else { return }

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

        // Rows that are not in the database yet, tinted the whole way across so
        // that the place the result ends and the new rows begin is legible
        // without counting.
        if !drafts.isEmpty {
            for (i, r) in rows.enumerated() where r >= table.rowCount {
                let y = rowY(i)
                guard y + rowHeight > headerHeight, y < viewH else { continue }
                fill(x: 0, y: y, w: viewW, h: rowHeight, color: Theme.Grid.draftRow.simd)
            }
        }

        // Rows on their way out, under the selection: the row somebody just
        // marked is the row they are standing on, and a selection band that hid
        // the mark would make the button they pressed look like it did nothing.
        if !deleted.isEmpty {
            for (i, r) in rows.enumerated() where deleted.contains(r) {
                let y = rowY(i)
                guard y + rowHeight > headerHeight, y < viewH else { continue }
                fill(x: 0, y: y, w: viewW, h: rowHeight, color: Theme.Grid.deletedRow.simd)
            }
        }

        // Selection, between the banding and the separators so that the band
        // reads as continuous across the row and the grid lines stay on top.
        if let selection {
            let selected = selection.rows
            for (i, r) in rows.enumerated() where selected.contains(r) {
                let y = rowY(i)
                guard y + rowHeight > headerHeight, y < viewH else { continue }
                fill(
                    x: 0, y: y, w: viewW, h: rowHeight,
                    color: Theme.Grid.selectedRow.simd)
            }
        }
        // Pending changes, under the cursor cell and over the banding: the mark
        // has to survive the row being selected, because the row somebody is
        // editing is the row they are standing on.
        if !pending.isEmpty {
            for (i, r) in rows.enumerated() {
                let y = rowY(i)
                guard y + rowHeight > headerHeight, y < viewH else { continue }
                for c in cols
                where !hiddenColumns.contains(c) && pending.contains(GridCell(row: r, column: c)) {
                    let cx = columnX(c) - scrollX
                    let cw = columnWidth(c)
                    guard cx + cw > 0, cx < viewW else { continue }
                    fill(x: cx, y: y, w: cw, h: rowHeight, color: Theme.Grid.pendingCell.simd)
                }
            }
        }

        // The cursor cell is drawn separately: within a multi-row selection it
        // is the one cell the keyboard and the inspector act on, so it needs to
        // stay distinguishable from the band around it.
        if let selection, rows.contains(selection.row), !hiddenColumns.contains(selection.column) {
            let y = rowY(selection.row - rows.lowerBound)
            if y + rowHeight > headerHeight, y < viewH {
                let cx = columnX(selection.column) - scrollX
                let cw = columnWidth(selection.column)
                if cx + cw > 0, cx < viewW {
                    fill(
                        x: cx, y: y, w: cw, h: rowHeight,
                        color: Theme.Grid.selectedCell.simd)
                    // A 1pt edge on the leading side marks the cell even where
                    // the fill sits over a dark value and washes out.
                    fill(
                        x: cx, y: y, w: 1, h: rowHeight,
                        color: Theme.Grid.cursor.simd)
                }
            }
        }

        // Column separators.
        for c in cols where !hiddenColumns.contains(c) {
            let x = columnOffsets[c + 1] - scrollX
            guard x >= 0, x < viewW else { continue }
            fill(
                x: x, y: headerHeight, w: 1, h: viewH - headerHeight,
                color: Theme.Grid.separator.simd)
        }

        // Header band, opaque so rows scroll beneath it.
        fill(x: 0, y: 0, w: viewW, h: headerHeight, color: Theme.Grid.header.simd)
        fill(
            x: 0, y: headerHeight - 1, w: viewW, h: 1,
            color: Theme.Grid.separator.simd)
        // Cells, column-major so per-column geometry is resolved once rather
        // than per cell. Draw order within the cell block does not matter: the
        // fills beneath them are already in the buffer.
        for c in cols where !hiddenColumns.contains(c) {
            let x = columnX(c) - scrollX
            let w = columnWidth(c)
            let maxChars = charsFitting(w)
            let alignRight = table.columns[c].kind.isNumeric

            // The sorted column's header is tinted as well as marked, so the
            // ordering is legible without resolving a 6pt triangle.
            let isSorted = sort?.column == c
            if isSorted, let sort {
                emitGlyph(
                    uv: sort.descending ? atlas.sortDescendingUV : atlas.sortAscendingUV,
                    x: x + w - cellPadding - atlas.advance, y: headerNameY,
                    color: Theme.Grid.cursor.simd)
            }
            emitCell(
                table.columns[c].name, x: x,
                // Never run the label under the marker.
                width: isSorted ? w - atlas.advance : w,
                maxChars: isSorted ? maxChars - 1 : maxChars, y: headerNameY,
                color: isSorted ? Theme.Grid.sortedHeaderText.simd : Theme.Grid.headerText.simd,
                alignRight: false)
            // The full width, unlike the name: the sort marker sits on the name's
            // line, so there is nothing on this one to keep clear of.
            emitCell(
                typeLabels[c], x: x, width: w, maxChars: maxChars, y: headerTypeY,
                color: Theme.Grid.headerType.simd, alignRight: false)

            for (i, r) in rows.enumerated() {
                let y = rowY(i)
                guard y < viewH, y + rowHeight > headerHeight else { continue }
                let (text, colour) = cellText(row: r, column: c)
                emitCell(
                    text, x: x, width: w, maxChars: maxChars, y: y + 3,
                    // Only a plain value is right-aligned. The words — NULL,
                    // DEFAULT, and the empty cell of a NOT NULL column — read as
                    // labels rather than as data, and each comes back in its own
                    // colour, which is what identifies them here.
                    color: colour, alignRight: alignRight && colour == Theme.Grid.text.simd)
            }
        }

        // Over the data, not beside it: reserving a gutter would shorten every
        // column measurement in this file for eleven points of chrome.
        emitScrollbars(viewSize: viewSize)

        if isFocused {
            let ring = Theme.Grid.cursor.opacity(0.7).simd
            fill(x: 0, y: 0, w: viewW, h: 1, color: ring)
            fill(x: 0, y: viewH - 1, w: viewW, h: 1, color: ring)
            fill(x: 0, y: 0, w: 1, h: viewH, color: ring)
            fill(x: viewW - 1, y: 0, w: 1, h: viewH, color: ring)
        }
    }

    /// What a cell says, and in which colour.
    ///
    /// The one place the rules live, because two readers need the same answer:
    /// the draw path above, and the accessibility tree that tells a screen reader
    /// what is on screen. A cell described differently from the way it is drawn
    /// would be a second, invisible version of the result.
    func cellText(row: Int, column: Int) -> (String, SIMD4<Float>) {
        guard let table, column < table.columns.count else { return ("", Theme.Grid.text.simd) }
        if row >= table.rowCount {
            let draft = row - table.rowCount
            guard draft < drafts.count else { return ("", Theme.Grid.text.simd) }
            return draftText(row: draft, column: column)
        }
        // NULL is spelled as the word, dimmed. `text` returns "" for both NULL
        // and an empty string, and presenting them the same way hides a
        // distinction the user is querying on.
        //
        // Unless the server declared the column NOT NULL, where the word would be
        // the lie instead: the only NULL that can reach such a column is one a
        // driver substituted for a value Arrow has no shape for. Measured on
        // MySQL 8.4 — the server clears its NOT NULL flag on the nullable side of
        // an outer join, so a result column that still carries it cannot hold a
        // NULL from the data.
        if table.isNull(row: row, column: column) {
            return (table.columns[column].declaredNotNull ? "" : "NULL", Theme.Grid.nullText.simd)
        }
        return (table.text(row: row, column: column), Theme.Grid.text.simd)
    }

    /// What a draft's cell says, and in which colour.
    ///
    /// Three states, because a new row has one more than a fetched row does: a
    /// value, an explicit NULL, and a column nobody typed into — which is left
    /// out of the INSERT so the table's own default applies. Drawn as the words
    /// they mean rather than as an empty cell, which would say the same thing
    /// about all three.
    private func draftText(row: Int, column: Int) -> (String, SIMD4<Float>) {
        guard let value = drafts[row].values[column] else {
            return ("DEFAULT", Theme.Grid.defaultText.simd)
        }
        guard let text = value.text else { return ("NULL", Theme.Grid.nullText.simd) }
        return (text, Theme.Grid.text.simd)
    }

    // MARK: - Hit testing

    /// The cell at a point in the coordinates this renderer draws in — y down from
    /// the top, which is what `GridView.rendererPoint` produces — or `nil` for the
    /// header and the empty area past the last row or column.
    ///
    /// It used to take the view's height as well and never read it. That unused
    /// parameter was the shape of a bug: the caller was passing a y measured from
    /// the bottom, and something here looked like it was accounting for the
    /// difference.
    func cell(at point: CGPoint, table: ArrowTable) -> GridSelection? {
        let y = Float(point.y)
        guard y >= headerHeight else { return nil }
        let row = Int(scrollRow + Double((y - headerHeight) / rowHeight))
        guard row >= 0, row < totalRows,
            let column = columnIndex(atX: Float(point.x) + scrollX)
        else { return nil }
        return GridSelection(row: row, column: column)
    }

    /// Scrolls the minimum distance that brings `selection` fully into view.
    ///
    /// Minimum rather than centred: keyboard navigation that recentres on every
    /// keystroke makes the surrounding rows impossible to track.
    func scrollToVisible(_ selection: GridSelection, viewSize: CGSize) {
        let visibleRows = Double(max(1, Int((Float(viewSize.height) - headerHeight) / rowHeight)))
        let row = Double(selection.row)
        if row < scrollRow {
            scrollRow = row
        } else if row >= scrollRow + visibleRows - 1 {
            scrollRow = row - visibleRows + 2
        }
        // The upper clamp matters as much as the lower one: overshooting by the
        // row or two this adds would leave blank space under the last row and
        // put the scrollbar's thumb past the end of its own track.
        scrollRow = max(0, min(scrollRow, maxScrollRow(viewSize: viewSize)))

        let left = columnX(selection.column)
        let width = columnWidth(selection.column)
        if left < scrollX {
            scrollX = left
        } else if left + width > scrollX + Float(viewSize.width) {
            scrollX = left + width - Float(viewSize.width)
        }
        scrollX = max(0, min(scrollX, max(0, contentWidth - Float(viewSize.width))))
    }

    private func fill(x: Float, y: Float, w: Float, h: Float, color: SIMD4<Float>) {
        let uv = atlas.solidUV
        instances.append(
            GlyphInstance(
                pos: SIMD2((x * scale).rounded(), (y * scale).rounded()),
                size: SIMD2((w * scale).rounded(), (h * scale).rounded()),
                uvOrigin: SIMD2(uv.x, uv.y),
                uvSize: SIMD2(uv.w, uv.h),
                color: color))
    }

    /// Draws one cell's text within `[x, x + width]`.
    ///
    /// A value too wide for its column ends in an ellipsis rather than simply
    /// stopping. Silent truncation is the worst failure this grid can have:
    /// `123456789` clipped to `12345` does not look truncated, it looks like a
    /// different number.
    private func emitCell(
        _ s: String, x: Float, width: Float, maxChars: Int, y: Float,
        color: SIMD4<Float>, alignRight: Bool
    ) {
        let count = s.utf8.count

        if count > maxChars {
            // Truncated values always start at the leading edge, even numeric
            // ones: right-aligning would cut the most significant digits.
            emit(text: s, x: x + cellPadding, y: y, color: color, maxChars: maxChars - 1)
            emitGlyph(
                uv: atlas.ellipsisUV,
                x: x + cellPadding + Float(maxChars - 1) * atlas.advance,
                y: y, color: color)
            return
        }

        let startX =
            alignRight
            ? x + width - cellPadding - Float(count) * atlas.advance
            : x + cellPadding
        emit(text: s, x: startX, y: y, color: color, maxChars: maxChars)
    }

    /// Appends one quad per visible glyph.
    ///
    /// Positions are snapped to whole device pixels and derived from the
    /// character index rather than an accumulating pen, so rounding cannot drift
    /// across a long cell and sampling stays 1:1 with the atlas texels.
    ///
    /// Non-ASCII bytes are skipped but still consume a column. Phase 0 data is
    /// ASCII; real text handling arrives with the value formatters in phase 4.
    private func emit(text: String, x: Float, y: Float, color: SIMD4<Float>, maxChars: Int) {
        var n = 0
        for byte in text.utf8 {
            if n >= maxChars { break }
            let index = n
            n += 1
            guard byte != 32, let uv = atlas.uv(for: byte) else { continue }
            emitGlyph(uv: uv, x: x + Float(index) * atlas.advance, y: y, color: color)
        }
    }

    private func emitGlyph(
        uv: (x: Float, y: Float, w: Float, h: Float), x: Float, y: Float,
        color: SIMD4<Float>
    ) {
        instances.append(
            GlyphInstance(
                pos: SIMD2(
                    (x * scale).rounded() - atlas.quadInset,
                    (y * scale).rounded() - atlas.quadInset),
                size: SIMD2(atlas.cellWidth, atlas.cellHeight),
                uvOrigin: SIMD2(uv.x, uv.y),
                uvSize: SIMD2(uv.w, uv.h),
                color: color))
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
