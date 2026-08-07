import AppKit
import MetalKit
import CDbFfi

// Kept out of main.swift deliberately: types declared in a file with top-level
// code inherit @MainActor isolation, which is wrong for a controller whose
// whole job is to move work off the main thread.

/// Raw Arrow pointers handed from the loading thread to the main thread.
/// Ownership transfers with the value and the receiver is the only reader, so
/// the unchecked conformance is a statement about the protocol, not a shortcut.
struct ArrowHandoff<T>: @unchecked Sendable {
    let pointer: UnsafeMutablePointer<T>
}

final class GridViewController: NSObject, MTKViewDelegate {
    let renderer: GridRenderer
    let table = ArrowTable()

    private let connString: String
    private let sql: String
    private let benchMode: Bool
    private let benchFrames: Int
    let verifyMode: Bool

    private var firstBatchMs: Double = .nan
    private var loadCompleteMs: Double = .nan
    private var benchFrameCount = 0
    private var benchStarted = false

    init(renderer: GridRenderer, connString: String, sql: String,
         benchMode: Bool, benchFrames: Int, verifyMode: Bool) {
        self.renderer = renderer
        self.connString = connString
        self.sql = sql
        self.benchMode = benchMode
        self.benchFrames = benchFrames
        self.verifyMode = verifyMode
        super.init()
        renderer.table = table
    }

    /// Prints where the data Swift reads actually lives.
    ///
    /// Buffers are reported for two separate batches: if a copy were happening,
    /// the addresses would land in Swift-owned allocations and RSS would carry a
    /// second copy of the entire result.
    private func printZeroCopyProbe() {
        for batchIdx in [0, max(0, 1)] where batchIdx < 2 {
            let probes = table.probe(batch: batchIdx)
            guard !probes.isEmpty else { continue }
            let totalMalloc = probes.reduce(0) { $0 + $1.mallocSize }
            print("probe_batch      \(batchIdx)")
            print("  columns        \(probes.count)")
            print("  rows           \(probes.first?.rows ?? 0)")
            print("  buffer_bytes   \(totalMalloc)")
            for p in probes.prefix(4) {
                print(String(format: "  %-14@ @0x%llx  alloc=%d",
                             p.column as NSString, UInt64(p.address), p.mallocSize))
            }
        }
    }

    /// `onReady` is `@MainActor` because it is only ever invoked from the main
    /// queue below, and callers legitimately want to touch the view from it.
    /// A plain `@Sendable` closure would compile but forbid exactly that.
    func loadInBackground(onReady: @escaping @MainActor () -> Void) {
        let connString = self.connString
        let sql = self.sql
        DispatchQueue.global(qos: .userInitiated).async { [self] in
            do {
                let loadStart = CFAbsoluteTimeGetCurrent()
                let db = try Database(connString: connString)
                let query = try db.query(sql, batchRows: 8192)

                let schema = ArrowHandoff(pointer: try query.schema())
                DispatchQueue.main.sync {
                    table.setSchema(schema.pointer)
                    if let release = schema.pointer.pointee.release {
                        release(schema.pointer)
                    }
                    schema.pointer.deallocate()
                }

                var isFirst = true
                while let raw = try query.nextBatch() {
                    if isFirst {
                        firstBatchMs = (CFAbsoluteTimeGetCurrent() - loadStart) * 1000
                        isFirst = false
                    }
                    // Serialized onto the main thread because the renderer reads
                    // the same table. Phase 1 hands across an immutable snapshot
                    // instead; here the cost shows up in load time and nowhere
                    // else, which is acceptable for a measurement harness.
                    let batch = ArrowHandoff(pointer: raw)
                    DispatchQueue.main.sync {
                        table.append(batch: batch.pointer)
                    }
                }
                loadCompleteMs = (CFAbsoluteTimeGetCurrent() - loadStart) * 1000

                let rows = table.rowCount
                let cols = table.columns.count
                let first = firstBatchMs
                let total = loadCompleteMs
                let wantProbe = self.verifyMode
                DispatchQueue.main.async {
                    print("rows             \(rows)")
                    print("columns          \(cols)")
                    print("first_batch_ms   \(String(format: "%.1f", first))")
                    print("load_total_ms    \(String(format: "%.1f", total))")
                    if wantProbe { self.printZeroCopyProbe() }
                    // Already on the main queue; assert that rather than hop
                    // again, which would delay the first frame by a runloop turn.
                    MainActor.assumeIsolated { onReady() }
                }
            } catch {
                print("load failed: \(error)")
                exit(1)
            }
        }
    }

    func mtkView(_ view: MTKView, drawableSizeWillChange size: CGSize) {}

    func draw(in view: MTKView) {
        if benchMode && benchStarted {
            // Advance across the whole result so the measurement covers cold
            // cells rather than repeatedly re-reading one page.
            let step = Double(max(1, table.rowCount / benchFrames))
            renderer.scrollRow += step
            if renderer.scrollRow > Double(table.rowCount) { renderer.scrollRow = 0 }
        }

        renderer.draw(in: view)

        if benchMode && benchStarted {
            benchFrameCount += 1
            if benchFrameCount >= benchFrames {
                report()
                exit(0)
            }
        }
    }

    /// Drives the benchmark's frames explicitly instead of leaving them to the
    /// display link.
    ///
    /// macOS throttles drawing for occluded and background windows down to a
    /// frame or two per second, so a display-link-driven bench silently depends
    /// on whether the window happens to be frontmost — it hangs when run from a
    /// script. Frame samples time the render call itself, never the wait
    /// between vsyncs, so pacing by the display link contributed nothing to the
    /// number in the first place.
    func startBench(view: MTKView) {
        // Skip the first frames: pipeline warm-up and the initial texture
        // upload are not steady-state scrolling.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [self] in
            renderer.resetStats()
            benchStarted = true
            while benchFrameCount < benchFrames {
                view.draw()
            }
        }
    }

    private func report() {
        let s = renderer.frameSamples.sorted()
        // A frame that could not acquire a drawable records no sample. Missing
        // samples mean the run measured less than it claims, so say so instead
        // of printing a confident average over whatever survived.
        guard s.count == benchFrames else {
            print("bench incomplete: \(s.count) of \(benchFrames) frames sampled")
            exit(1)
        }
        func pct(_ p: Double) -> Double { s[min(s.count - 1, Int(Double(s.count) * p))] }
        let mean = s.reduce(0, +) / Double(s.count)
        print("frames           \(s.count)")
        print("frame_mean_ms    \(String(format: "%.3f", mean))")
        print("frame_p50_ms     \(String(format: "%.3f", pct(0.50)))")
        print("frame_p95_ms     \(String(format: "%.3f", pct(0.95)))")
        print("frame_p99_ms     \(String(format: "%.3f", pct(0.99)))")
        print("frame_max_ms     \(String(format: "%.3f", s.last!))")
        print("implied_fps      \(String(format: "%.0f", 1000.0 / mean))")
    }
}

/// MTKView subclass carrying the grid's pointer and keyboard interaction.
///
/// The grid is the surface a user spends the session in, so it has to answer
/// clicks, arrow keys, and ⌘C the way every other table on the platform does.
/// None of that can come from SwiftUI: the content is drawn, not composed of
/// views, so there is nothing for SwiftUI to hit-test or focus.
final class GridView: MTKView {
    weak var renderer: GridRenderer?

    /// Called when the selected cell changes, so the chrome can show the full
    /// value and the status bar can show where the cursor is.
    var onSelect: ((GridSelection) -> Void)?

    /// Whether this grid takes keyboard focus when it appears. Set only for the
    /// browse pane: in the Query tab focus belongs to the editor, and a grid
    /// that grabs it on every tab switch is worse than one that never does.
    var claimsInitialFocus = false

    /// Column being resized by a header drag, with the geometry the drag started
    /// from — widths follow the pointer's total travel rather than accumulating
    /// per-event deltas, which drift.
    private var resizing: (column: Int, startX: CGFloat, startWidth: Float)?
    /// How close to a boundary counts as grabbing it. Wide enough to hit with a
    /// mouse, narrow enough not to swallow clicks meant for the header itself.
    private let edgeTolerance: Float = 4

    private var trackingArea: NSTrackingArea?

    override var acceptsFirstResponder: Bool { true }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let trackingArea { removeTrackingArea(trackingArea) }
        // Needed for the resize cursor: without mouse-moved events there is no
        // way to show that a boundary is grabbable before it is grabbed.
        let area = NSTrackingArea(
            rect: bounds,
            options: [.activeInKeyWindow, .mouseMoved, .mouseEnteredAndExited, .inVisibleRect],
            owner: self)
        addTrackingArea(area)
        trackingArea = area
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        guard claimsInitialFocus, let window else { return }
        // Deferred by one turn: SwiftUI installs its own first responder while
        // the hosting view lays out, and claiming focus before that happens
        // just loses the race. Without this the window opens with the caret in
        // the filter field, which reads as a text-entry app.
        DispatchQueue.main.async { [weak self] in
            guard let self, self.window === window else { return }
            window.makeFirstResponder(self)
        }
    }

    override func becomeFirstResponder() -> Bool {
        renderer?.isFocused = true
        needsDisplay = true
        return super.becomeFirstResponder()
    }

    override func resignFirstResponder() -> Bool {
        renderer?.isFocused = false
        needsDisplay = true
        return super.resignFirstResponder()
    }

    override func scrollWheel(with event: NSEvent) {
        guard let renderer, let table = renderer.table else { return }

        renderer.scrollRow = max(0, min(
            Double(table.rowCount - 1),
            renderer.scrollRow - Double(event.scrollingDeltaY) / Double(renderer.rowHeight) * 3))

        // Clamp so the last column cannot be scrolled off the right edge.
        let maxX = max(0, renderer.contentWidth - Float(bounds.width))
        renderer.scrollX = max(0, min(maxX, renderer.scrollX - Float(event.scrollingDeltaX)))

        needsDisplay = true
    }

    // MARK: - Pointer

    override func mouseMoved(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        if resizeTarget(at: point) != nil {
            NSCursor.resizeLeftRight.set()
        } else {
            NSCursor.arrow.set()
        }
    }

    override func mouseExited(with event: NSEvent) {
        NSCursor.arrow.set()
    }

    override func mouseDown(with event: NSEvent) {
        guard let renderer, let table = renderer.table else { return }
        window?.makeFirstResponder(self)
        let point = convert(event.locationInWindow, from: nil)

        if let column = resizeTarget(at: point) {
            resizing = (column, point.x, renderer.columnWidth(column))
            return
        }

        guard let hit = renderer.cell(at: point, viewHeight: bounds.height, table: table)
        else { return }
        apply(hit)
    }

    override func mouseDragged(with event: NSEvent) {
        guard let resizing, let renderer else { return }
        let point = convert(event.locationInWindow, from: nil)
        renderer.setColumnWidth(
            resizing.startWidth + Float(point.x - resizing.startX), at: resizing.column)
        needsDisplay = true
    }

    override func mouseUp(with event: NSEvent) {
        resizing = nil
    }

    /// The column whose trailing edge is under `point`, if the point is in the
    /// header. Resize handles live only there, so a drag inside the data area is
    /// never mistaken for one.
    private func resizeTarget(at point: CGPoint) -> Int? {
        guard let renderer, Float(point.y) < renderer.headerHeight else { return nil }
        return renderer.columnEdge(
            nearX: Float(point.x) + renderer.scrollX, tolerance: edgeTolerance)
    }

    override func keyDown(with event: NSEvent) {
        guard let renderer, let table = renderer.table, table.rowCount > 0 else { return }

        // ⌘C on the selected cell. Routed here rather than through the Edit menu
        // because the grid is not a text view and has no field editor to answer
        // the standard copy: selector for it.
        if event.modifierFlags.contains(.command),
           event.charactersIgnoringModifiers?.lowercased() == "c" {
            copySelection()
            return
        }

        let current = renderer.selection ?? GridSelection(row: Int(renderer.scrollRow), column: 0)
        let lastRow = table.rowCount - 1
        let lastColumn = max(0, table.columns.count - 1)
        let page = max(1, Int(
            (bounds.height - CGFloat(renderer.headerHeight)) / CGFloat(renderer.rowHeight)) - 1)

        var next = current
        switch event.specialKey {
        case .upArrow: next.row -= 1
        case .downArrow: next.row += 1
        case .leftArrow: next.column -= 1
        case .rightArrow: next.column += 1
        case .pageUp: next.row -= page
        case .pageDown: next.row += page
        case .home: next.row = 0
        case .end: next.row = lastRow
        default:
            // Silently ignored rather than passed to `super`, which would beep:
            // typing into a read-only grid is a slip, not an error worth a sound.
            return
        }

        next.row = min(max(0, next.row), lastRow)
        next.column = min(max(0, next.column), lastColumn)
        apply(next)
        renderer.scrollToVisible(next, viewSize: bounds.size)
    }

    private func apply(_ selection: GridSelection) {
        renderer?.selection = selection
        needsDisplay = true
        onSelect?(selection)
    }

    private func copySelection() {
        guard let renderer, let table = renderer.table, let selection = renderer.selection
        else { return }
        // NULL copies as an empty string, not as the word: what is on the
        // pasteboard should be the value, not the way the grid spells it.
        let value = table.isNull(row: selection.row, column: selection.column)
            ? "" : table.text(row: selection.row, column: selection.column)
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(value, forType: .string)
    }
}
