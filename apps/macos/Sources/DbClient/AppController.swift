import AppKit
import CDbFfi
import MetalKit

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

    init(
        renderer: GridRenderer, connString: String, sql: String,
        benchMode: Bool, benchFrames: Int, verifyMode: Bool
    ) {
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
                print(
                    String(
                        format: "  %-14@ @0x%llx  alloc=%d",
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
/// What a filter offered over one cell can ask, spelled as the core's JSON.
///
/// The raw values are the wire: `db_cell_filter` reads exactly these four words,
/// and a fifth spelling here would be a request the core rejects at run time
/// rather than a mistake the compiler catches.
enum CellFilterOperator: String, Encodable, Sendable {
    case equals
    case notEquals = "not_equals"
    case isNull = "is_null"
    case isNotNull = "is_not_null"
}

/// One cell, and what a menu item asks about it.
///
/// Carried on the menu item rather than read back off the grid when the item is
/// chosen: a menu stays open across events, and the cell it was built for is the
/// cell it must act on.
struct CellFilterRequest: Sendable {
    let column: String
    /// The cell's text, or nil where it holds NULL — which the core turns into
    /// `IS NULL` rather than into the `= NULL` that is never true.
    let value: String?
    let op: CellFilterOperator
    /// Whether the clause is ANDed onto the filter field or replaces it.
    let extend: Bool
}

final class GridView: MTKView {
    weak var renderer: GridRenderer?

    /// Called when the selected cell changes, so the chrome can show the full
    /// value and the status bar can show where the cursor is.
    var onSelect: ((GridSelection) -> Void)?

    /// Called when a header is clicked away from a resize handle.
    var onHeaderClick: ((Int) -> Void)?
    /// Whether headers respond to a click at all. Drives the cursor as well as
    /// the action: a pointing hand over a header that does nothing is a lie.
    var sortsOnHeaderClick = false

    /// Called when a filter is chosen from the context menu.
    var onFilter: ((CellFilterRequest) -> Void)?
    /// Whether the menu offers filters at all. False for the Query pane: its
    /// result can join five tables, and there is no answer to which of them a
    /// column belongs to that is right often enough to write into a WHERE
    /// clause — the same reason that pane cannot be edited or sorted.
    var offersFilters = false

    /// Called with the hit rows when *Copy as INSERT* is chosen.
    var onCopyAsInsert: ((ClosedRange<Int>) -> Void)?
    /// Whether that item is offered. False for the Query pane, for the reason
    /// above: an INSERT names a table, and a result joining five of them names
    /// none.
    var offersInsertCopy = false

    /// Whether this grid takes keyboard focus when it appears. Set only for the
    /// browse pane: in the Query tab focus belongs to the editor, and a grid
    /// that grabs it on every tab switch is worse than one that never does.
    var claimsInitialFocus = false

    /// What this grid is called out loud. Set from SwiftUI rather than fixed
    /// here: the window has two of these, and "Query result grid" is how a
    /// screen reader tells them apart.
    var accessibilityName = "Result grid"

    /// The rows and cells a screen reader walks. Lazy because it holds this view
    /// as its source, and `self` cannot be handed out from a property
    /// initialiser; nothing but the accessibility system touches it, so a grid
    /// nobody inspects never builds one.
    lazy var accessibilityTree = GridAccessibilityTree(source: self, container: self)

    /// Column being resized by a header drag, with the geometry the drag started
    /// from — widths follow the pointer's total travel rather than accumulating
    /// per-event deltas, which drift.
    private var resizing: (column: Int, startX: CGFloat, startWidth: Float)?
    /// How close to a boundary counts as grabbing it. Wide enough to hit with a
    /// mouse, narrow enough not to swallow clicks meant for the header itself.
    private let edgeTolerance: Float = 4

    /// Scrollbar drag in progress, with where inside the thumb it was grabbed —
    /// so the thumb stays under the pointer instead of jumping its own length on
    /// the first move.
    private var scrollDrag: (axis: GridRenderer.ScrollAxis, grabOffset: Float)?

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
        guard let renderer, renderer.table != nil else { return }

        // Clamped so the last row lands at the bottom rather than scrolling up
        // into blank space. It is also what lets the scrollbar reach its end
        // exactly when the data does.
        renderer.scrollRow = max(
            0,
            min(
                renderer.maxScrollRow(viewSize: bounds.size),
                renderer.scrollRow - Double(event.scrollingDeltaY) / Double(renderer.rowHeight) * 3)
        )

        renderer.scrollX = max(
            0,
            min(
                renderer.maxScrollX(viewWidth: bounds.width),
                renderer.scrollX - Float(event.scrollingDeltaX)))

        needsDisplay = true
    }

    // MARK: - Pointer

    /// Where an event landed, in the coordinates the renderer draws in: y measured
    /// down from the top of the view.
    ///
    /// The renderer puts y = 0 at the top — that is where it fills the header band,
    /// with the first row below it — while an `MTKView` is not flipped, so a mouse
    /// event's y arrives measured up from the bottom. Handing that straight to the
    /// renderer mirrored every pointer answer it gave: a click on the first row
    /// selected one counted from the other end, a click on a header did nothing
    /// while the bottom 32pt sorted, and both scrollbar gutters were on the wrong
    /// edge. Converted once here rather than inside each of `cell(at:)`,
    /// `isInHeader` and the scrollbar geometry, because it is one fact about the
    /// view and not three.
    static func rendererPoint(of viewPoint: CGPoint, viewHeight: CGFloat) -> CGPoint {
        CGPoint(x: viewPoint.x, y: viewHeight - viewPoint.y)
    }

    private func rendererPoint(of event: NSEvent) -> CGPoint {
        Self.rendererPoint(
            of: convert(event.locationInWindow, from: nil), viewHeight: bounds.height)
    }

    override func mouseMoved(with event: NSEvent) {
        let point = rendererPoint(of: event)
        if resizeTarget(at: point) != nil {
            NSCursor.resizeLeftRight.set()
        } else if sortsOnHeaderClick, isInHeader(point) {
            NSCursor.pointingHand.set()
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
        let point = rendererPoint(of: event)

        // Before anything else: the gutters sit over the data, so a click there
        // must not also land on the cell underneath.
        if let axis = renderer.scrollbarAxis(at: point, viewSize: bounds.size),
            let metrics = renderer.scrollbar(axis, viewSize: bounds.size)
        {
            let coord = renderer.scrollbarCoordinate(axis, of: point)
            let onThumb =
                coord >= metrics.thumbStart
                && coord <= metrics.thumbStart + metrics.thumbLength
            // A click on the track goes where it points rather than paging
            // towards it. On a million rows, paging there takes all afternoon.
            let grabOffset = onThumb ? coord - metrics.thumbStart : metrics.thumbLength / 2
            scrollDrag = (axis, grabOffset)
            renderer.activeScrollAxis = axis
            renderer.scrollTo(axis, thumbStart: coord - grabOffset, viewSize: bounds.size)
            needsDisplay = true
            return
        }

        if let column = resizeTarget(at: point) {
            resizing = (column, point.x, renderer.columnWidth(column))
            return
        }

        if isInHeader(point) {
            if sortsOnHeaderClick,
                let column = renderer.columnIndex(atX: Float(point.x) + renderer.scrollX)
            {
                onHeaderClick?(column)
            }
            return
        }

        guard var hit = renderer.cell(at: point, table: table)
        else { return }
        // Shift-click extends from wherever the range already starts, so a
        // second shift-click re-aims the same range instead of chaining a new
        // one off the last click.
        if event.modifierFlags.contains(.shift), let current = renderer.selection {
            hit.anchor = current.anchor ?? current.row
        }
        apply(hit)
    }

    /// The right-click menu. It offers to copy only what the click actually
    /// hit: a menu offering to copy a cell that was not clicked is worse than
    /// no menu, so the guards are the ones `mouseDown` applies, in the same
    /// order.
    override func menu(for event: NSEvent) -> NSMenu? {
        guard let renderer, let table = renderer.table else { return nil }
        let point = rendererPoint(of: event)

        if renderer.scrollbarAxis(at: point, viewSize: bounds.size) != nil { return nil }
        if resizeTarget(at: point) != nil { return nil }
        if isInHeader(point) { return nil }

        guard var hit = renderer.cell(at: point, table: table) else { return nil }
        // Shift-right-click extends from wherever the range already starts, the
        // way a shift-click does, so the menu can offer to copy more than the
        // one cell under the pointer.
        if event.modifierFlags.contains(.shift), let current = renderer.selection {
            hit.anchor = current.anchor ?? current.row
        }
        // AppKit shows the menu after this returns, so the selection is moved
        // first: the menu and the outline then agree about which cell is being
        // acted on.
        apply(hit)

        // Counted from the hit rather than read back off the renderer, so this
        // does not depend on `apply` having stored what it was given.
        let count = hit.rows.count
        let menu = NSMenu()
        let value = NSMenuItem(title: "Copy Value", action: #selector(copyValue), keyEquivalent: "")
        value.target = self
        let rows = NSMenuItem(
            title: "Copy \(AppModel.pluralized(count, "row"))",
            action: #selector(copyRows), keyEquivalent: "")
        rows.target = self
        let csv = NSMenuItem(
            title: "Copy \(AppModel.pluralized(count, "row")) as CSV",
            action: #selector(copyRowsAsCSV), keyEquivalent: "")
        csv.target = self
        menu.addItem(value)
        menu.addItem(rows)
        menu.addItem(csv)
        if offersInsertCopy {
            let insert = NSMenuItem(
                title: "Copy \(AppModel.pluralized(count, "row")) as INSERT",
                action: #selector(copyRowsAsInsert), keyEquivalent: "")
            insert.target = self
            menu.addItem(insert)
        }

        // A draft row is not offered any of this: it holds what somebody is
        // typing, the database has never seen it, and every cell of it the grid
        // can read back reads as empty.
        if offersFilters, hit.column < table.columnNames.count, hit.row < table.rowCount {
            let column = table.columnNames[hit.column]
            // Read from the table rather than through `GridClipboard.value`,
            // which renders NULL as an empty string. Here the difference is the
            // whole question: an empty text cell and an absent value are
            // different rows, and they filter differently.
            let cell = table.value(row: hit.row, column: hit.column)
            menu.addItem(.separator())
            menu.addItem(
                filters(titled: "Filter on \(column)", column: column, value: cell, extend: false))
            menu.addItem(
                filters(titled: "Add to Filter", column: column, value: cell, extend: true))
        }
        return menu
    }

    /// One submenu of predicates over the clicked cell.
    ///
    /// Two submenus rather than eight items in a row: replacing the filter and
    /// adding to it are the same four questions asked of the same cell, and a
    /// flat list of eight would leave the reader working out which half they are
    /// looking at.
    ///
    /// The two NULL entries are spelled the way SQL spells them, because that is
    /// what lands in the filter field — somebody who then edits it by hand is
    /// reading SQL, not this menu.
    private func filters(
        titled title: String, column: String, value: String?, extend: Bool
    ) -> NSMenuItem {
        let parent = NSMenuItem(title: title, action: nil, keyEquivalent: "")
        let submenu = NSMenu(title: title)
        let offers: [(String, CellFilterOperator)] = [
            ("Equals This Value", .equals),
            ("Does Not Equal This Value", .notEquals),
            ("IS NULL", .isNull),
            ("IS NOT NULL", .isNotNull)
        ]
        for (name, op) in offers {
            let item = NSMenuItem(
                title: name, action: #selector(applyCellFilter), keyEquivalent: "")
            item.target = self
            item.representedObject = CellFilterRequest(
                column: column, value: value, op: op, extend: extend)
            submenu.addItem(item)
        }
        parent.submenu = submenu
        return parent
    }

    @objc private func applyCellFilter(_ sender: NSMenuItem) {
        guard let request = sender.representedObject as? CellFilterRequest else { return }
        onFilter?(request)
    }

    /// Hands the selected rows off to be rendered as statements.
    ///
    /// Unlike the three copies above it, this one does not put anything on the
    /// pasteboard itself: the statements are written by the core, which means a
    /// round trip, which means the answer arrives after this returns.
    @objc private func copyRowsAsInsert(_ sender: Any?) {
        guard let selection = renderer?.selection else { return }
        onCopyAsInsert?(selection.rows)
    }

    override func mouseDragged(with event: NSEvent) {
        guard let renderer else { return }
        let point = rendererPoint(of: event)

        if let scrollDrag {
            renderer.scrollTo(
                scrollDrag.axis,
                thumbStart: renderer.scrollbarCoordinate(scrollDrag.axis, of: point)
                    - scrollDrag.grabOffset,
                viewSize: bounds.size)
            needsDisplay = true
            return
        }

        guard let resizing else { return }
        renderer.setColumnWidth(
            resizing.startWidth + Float(point.x - resizing.startX), at: resizing.column)
        needsDisplay = true
    }

    override func mouseUp(with event: NSEvent) {
        resizing = nil
        scrollDrag = nil
        renderer?.activeScrollAxis = nil
        needsDisplay = true
    }

    /// The column whose trailing edge is under `point`, if the point is in the
    /// header. Resize handles live only there, so a drag inside the data area is
    /// never mistaken for one.
    private func resizeTarget(at point: CGPoint) -> Int? {
        guard let renderer, isInHeader(point) else { return nil }
        return renderer.columnEdge(
            nearX: Float(point.x) + renderer.scrollX, tolerance: edgeTolerance)
    }

    private func isInHeader(_ point: CGPoint) -> Bool {
        guard let renderer else { return false }
        return point.y >= 0 && Float(point.y) < renderer.headerHeight
    }

    override func keyDown(with event: NSEvent) {
        guard let renderer, let table = renderer.table, renderer.totalRows > 0 else { return }

        let current = renderer.selection ?? GridSelection(row: Int(renderer.scrollRow), column: 0)
        // Counts the draft rows too, so the keyboard reaches a row that has just
        // been added the same way the mouse does.
        let lastRow = renderer.totalRows - 1
        let lastColumn = max(0, table.columns.count - 1)
        let page = max(
            1,
            Int(
                (bounds.height - CGFloat(renderer.headerHeight)) / CGFloat(renderer.rowHeight)) - 1)

        // ⌘C and ⌘A are routed here rather than through the Edit menu because
        // the grid is not a text view and has no field editor to answer the
        // standard copy:/selectAll: selectors for it.
        if event.modifierFlags.contains(.command) {
            switch event.charactersIgnoringModifiers?.lowercased() {
            case "c": copySelection()
            case "a": apply(GridSelection(row: lastRow, column: current.column, anchor: 0))
            default: break
            }
            return
        }

        let extending = event.modifierFlags.contains(.shift)
        var next = current
        switch event.specialKey {
        case .upArrow: next.row -= 1
        case .downArrow: next.row += 1
        // Past any column the grid is not drawing, so one press of the key moves
        // the cursor one column the user can see. Stopping on a hidden column
        // would read as a key that sometimes does nothing.
        case .leftArrow: next.column = renderer.drawnColumn(from: next.column - 1, step: -1)
        case .rightArrow: next.column = renderer.drawnColumn(from: next.column + 1, step: 1)
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
        // Shift keeps the range's fixed end where it was; an unshifted key drops
        // it, which is how every other list on the platform collapses a range.
        next.anchor = extending ? (current.anchor ?? current.row) : nil
        apply(next)
        renderer.scrollToVisible(next, viewSize: bounds.size)
    }

    /// Moves the cursor and tells everyone who follows it. Not private: the
    /// accessibility tree moves the same cursor, and a second way to set it would
    /// be a second cursor.
    func apply(_ selection: GridSelection) {
        renderer?.selection = selection
        needsDisplay = true
        onSelect?(selection)
        // The drawn cursor is the only thing that moves on its own; a screen
        // reader has to be told, or it goes on reading the cell the user has
        // already arrowed away from.
        if let cell = accessibilityTree.focusedCell() {
            NSAccessibility.post(element: cell, notification: .focusedUIElementChanged)
        }
        NSAccessibility.post(element: self, notification: .selectedRowsChanged)
    }

    /// Called when the result is replaced or grows. The elements read their
    /// values live, so nothing has to be rebuilt to be correct — but a screen
    /// reader caches the row count, and a table that has just gained 200 rows or
    /// become a different table has to say so.
    func resultDidChange() {
        accessibilityTree.invalidate()
        NSAccessibility.post(element: self, notification: .rowCountChanged)
        NSAccessibility.post(element: self, notification: .selectedRowsChanged)
    }

    // MARK: - Accessibility

    // The grid is drawn, so there is nothing here for the accessibility system to
    // discover on its own: every row and cell it can reach is one of these
    // answers. `GridAccessibilityTree` holds the elements; this is the table they
    // hang from.

    override func isAccessibilityElement() -> Bool { true }

    override func accessibilityRole() -> NSAccessibility.Role? { .table }

    override func accessibilityLabel() -> String? { accessibilityName }

    override func accessibilityRowCount() -> Int { accessibilityTree.rowCount }

    override func accessibilityColumnCount() -> Int { accessibilityTree.columns.count }

    override func accessibilityRows() -> [Any]? { accessibilityTree.boundedRows() }

    override func accessibilityChildren() -> [Any]? { accessibilityTree.boundedRows() }

    override func accessibilitySelectedRows() -> [Any]? {
        accessibilityTree.boundedSelectedRows()
    }

    override func accessibilityVisibleRows() -> [Any]? { accessibilityTree.visibleRows() }

    override func accessibilitySelectedCells() -> [Any]? {
        accessibilityTree.focusedCell().map { [$0] } ?? []
    }

    /// The cell the cursor is on, so focusing the grid reads that cell rather
    /// than announcing a table and stopping there. Not an override: `NSView`
    /// leaves this one to the accessibility protocol rather than implementing it.
    func accessibilityFocusedUIElement() -> Any? {
        accessibilityTree.focusedCell() ?? self
    }

    /// Both halves of AppKit's own answer to a table too large to hand over
    /// whole. A browse holds up to a million rows; this is what lets a screen
    /// reader ask for the forty it is about to read instead of all of them.
    override func accessibilityArrayAttributeCount(_ attribute: NSAccessibility.Attribute) -> Int {
        switch attribute {
        case .rows, .children: return accessibilityTree.rowCount
        case .visibleRows: return accessibilityTree.visibleRows().count
        case .selectedRows: return accessibilityTree.selectedRowCount
        default: return super.accessibilityArrayAttributeCount(attribute)
        }
    }

    override func accessibilityArrayAttributeValues(
        _ attribute: NSAccessibility.Attribute, index: Int, maxCount: Int
    ) -> [Any] {
        switch attribute {
        case .rows, .children: return accessibilityTree.rows(from: index, maxCount: maxCount)
        case .visibleRows:
            return Array(accessibilityTree.visibleRows().dropFirst(index).prefix(maxCount))
        case .selectedRows:
            return accessibilityTree.selectedRows(from: index, maxCount: maxCount)
        default:
            return super.accessibilityArrayAttributeValues(
                attribute, index: index, maxCount: maxCount)
        }
    }

    private func copySelection() {
        copy { table, selection in
            selection.rows.count > 1
                ? GridClipboard.tabSeparated(table, rows: selection.rows)
                : GridClipboard.value(of: table, row: selection.row, column: selection.column)
        }
    }

    /// The three items of the right-click menu, which differ from ⌘C and from
    /// each other only in which rendering they ask for.
    @objc private func copyValue() {
        copy { GridClipboard.value(of: $0, row: $1.row, column: $1.column) }
    }

    @objc private func copyRows() {
        copy { GridClipboard.tabSeparated($0, rows: $1.rows) }
    }

    @objc private func copyRowsAsCSV() {
        copy { GridClipboard.csv($0, rows: $1.rows) }
    }

    /// Puts one rendering of the selection on the pasteboard, or does nothing
    /// where there is no selection to render.
    ///
    /// The four callers above share this rather than each spelling the two
    /// pasteboard calls: `clearContents()` is what takes ownership, and a copy
    /// that set a string without it would leave the previous owner's other
    /// representations in place for the next paste to find.
    private func copy(_ render: (ArrowTable, GridSelection) -> String) {
        guard let renderer, let table = renderer.table, let selection = renderer.selection
        else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(render(table, selection), forType: .string)
    }
}
