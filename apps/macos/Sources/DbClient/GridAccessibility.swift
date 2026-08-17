import AppKit

/// The result grid, as a screen reader sees it.
///
/// The grid is a Metal-drawn `MTKView`, which means it had no accessibility
/// representation at all — not an approximate one. Every other surface in this
/// window is reachable with VoiceOver: the object tree, the tabs, the filter
/// fields, the cell editor. The data was not, which made the one thing this
/// application exists to show the one thing it could not say.
///
/// Three pieces do it. `GridAccessibilitySource` is what the tree reads, so this
/// file needs neither a Metal device nor a database to be exercised;
/// `GridAccessibilityTree` owns the elements and hands out slices of them; the
/// two element classes answer for one row and one cell each, reading through to
/// the source every time rather than holding a copy of a value that the next
/// fetch can change underneath them.

/// One column of the grid, as the tree refers to it.
struct GridAccessibleColumn: Equatable {
    /// The index the rest of the grid knows this column by — into
    /// `ArrowTable.columns`, which is what `GridSelection.column` means.
    let index: Int
    /// Where it sits among the columns actually being drawn, which is what a
    /// screen reader announces as the column number. The two differ as soon as
    /// one column is hidden.
    let position: Int
    let name: String
}

/// What the accessibility tree needs to know about a grid.
///
/// A protocol rather than a reference to `GridView`, and not for the sake of an
/// interface: a `GridView` needs a Metal device and its rows need a database,
/// while everything that can be wrong here — an off-by-one row number, a value
/// read from the wrong column, a cursor that does not follow — needs neither. It
/// is what makes `--verify-accessibility` a check that runs anywhere.
@MainActor
protocol GridAccessibilitySource: AnyObject {
    /// Rows the grid is showing, draft rows included: a row that has just been
    /// added is on screen, and a tree that stopped at the fetched ones would put
    /// it out of reach of the only user who cannot see it there.
    var accessibleRowCount: Int { get }

    /// The columns being drawn, in draw order. A hidden column is left out
    /// rather than announced as empty — the tree describes what is on screen.
    var accessibleColumns: [GridAccessibleColumn] { get }

    /// The rows on screen right now, for `AXVisibleRows`.
    var accessibleVisibleRows: Range<Int> { get }

    /// The cursor, and the rows a shift-extended selection covers.
    var accessibleCursor: GridSelection? { get }

    /// What a cell says, spelled the way the grid draws it — NULL and DEFAULT as
    /// the words they are. Read live rather than cached: a page arriving or a
    /// value being staged changes what a cell says without changing which cell
    /// it is.
    func accessibleText(row: Int, column: Int) -> String

    /// Moves the cursor to a cell and brings it on screen. What a screen reader
    /// navigating the table does, and it has to move the same cursor the
    /// keyboard and the mouse move — the cell inspector below the grid reads it,
    /// and two cursors would let it describe a cell nobody is on.
    func accessibleSelect(row: Int, column: Int)

    /// Where a cell is on screen, in screen coordinates, for the box a screen
    /// reader draws around what it is reading. `.zero` for anything scrolled out
    /// of sight: there is nothing to point at until `accessibleSelect` brings it
    /// in, which is what focusing it does.
    func accessibleFrame(row: Int, column: Int?) -> NSRect
}

/// The elements a screen reader walks, made as it asks for them.
///
/// The laziness is the whole design. A browse holds up to a million rows, and a
/// million row elements made up front is a hundred megabytes and a stall to
/// produce them — for a table whose reader is going to walk a screenful. So rows
/// are made when they are first asked for, cached by index, and the cache is
/// bounded: `GridView` answers `AXRows` through
/// `accessibilityArrayAttributeValues`, which is AppKit's own hook for exactly
/// this and asks for a slice at a time.
@MainActor
final class GridAccessibilityTree {
    /// Weak because the source is normally the view that owns this tree.
    private weak var source: (any GridAccessibilitySource)?
    /// The element every row and cell names as its parent, so a screen reader
    /// walking up from a cell arrives at the grid.
    private weak var container: NSView?

    private var rowElements: [Int: GridRowAccessibilityElement] = [:]
    /// The columns the cached elements were made against. A result with different
    /// columns is a different table, and its cells must not be described by the
    /// previous one's names.
    private var cachedColumns: [GridAccessibleColumn] = []

    /// How many row elements are kept. Chosen to be far more than a screen
    /// reader has on screen and far less than a result holds; past it the cache
    /// is dropped whole rather than evicted one at a time, because the elements
    /// are cheap to remake and an eviction policy is not free to maintain.
    static let cacheBound = 4096

    /// How many row elements are being held. Exposed for `--verify-accessibility`,
    /// because "a million rows do not become a million objects" is the design of
    /// this file and there is no other way to assert it from outside.
    var elementsMade: Int { rowElements.count }

    init(source: any GridAccessibilitySource, container: NSView?) {
        self.source = source
        self.container = container
    }

    var rowCount: Int { source?.accessibleRowCount ?? 0 }

    var columns: [GridAccessibleColumn] { source?.accessibleColumns ?? [] }

    /// Rows `index..<index + maxCount`, clamped to what there is.
    func rows(from index: Int, maxCount: Int) -> [Any] {
        guard index >= 0, maxCount > 0 else { return [] }
        let end = min(rowCount, index + maxCount)
        guard index < end else { return [] }
        return (index..<end).compactMap(row(_:))
    }

    /// What a client that asks for the whole array at once gets: as many rows as
    /// the cache holds.
    ///
    /// Bounded rather than complete, and this is the one place that trade is made.
    /// A screen reader reads `AXRows` through the count-and-slice pair below and
    /// reaches every row of a million-row result that way. A client that asks for
    /// the array in one go instead would have this build a million row elements
    /// and ten million cells inside them — minutes of allocation and gigabytes,
    /// in the accessibility path, which is to say a hang of the whole
    /// application. The row *count* stays the true one, so nothing here claims the
    /// table is smaller than it is.
    func boundedRows() -> [Any] { rows(from: 0, maxCount: Self.cacheBound) }

    func row(_ index: Int) -> GridRowAccessibilityElement? {
        guard let source, index >= 0, index < rowCount else { return nil }
        let columns = source.accessibleColumns
        if columns != cachedColumns {
            rowElements.removeAll(keepingCapacity: true)
            cachedColumns = columns
        }
        if let existing = rowElements[index] { return existing }
        if rowElements.count >= Self.cacheBound { rowElements.removeAll(keepingCapacity: true) }
        let element = GridRowAccessibilityElement(
            row: index, columns: columns, source: source, container: container)
        rowElements[index] = element
        return element
    }

    /// The rows a shift-extended selection covers.
    ///
    /// Counted from the range rather than from elements: ⌘A over a browse selects
    /// every row there is, and answering "how many are selected" must not build one
    /// object per row to find out.
    var selectedRowCount: Int { source?.accessibleCursor?.rows.count ?? 0 }

    func selectedRows(from index: Int, maxCount: Int) -> [Any] {
        guard let rows = source?.accessibleCursor?.rows, index >= 0, maxCount > 0 else { return [] }
        return rows.dropFirst(index).prefix(maxCount).compactMap(row(_:))
    }

    /// Bounded for the reason `boundedRows` is: a select-all is the case where
    /// this array is the whole result.
    func boundedSelectedRows() -> [Any] { selectedRows(from: 0, maxCount: Self.cacheBound) }

    func visibleRows() -> [Any] {
        guard let visible = source?.accessibleVisibleRows else { return [] }
        return visible.compactMap(row(_:))
    }

    /// The cell the cursor is on. What a screen reader reads when the grid takes
    /// focus, and what it re-reads when an arrow key moves the cursor.
    func focusedCell() -> GridCellAccessibilityElement? {
        guard let cursor = source?.accessibleCursor else { return nil }
        return cell(row: cursor.row, column: cursor.column)
    }

    func cell(row index: Int, column: Int) -> GridCellAccessibilityElement? {
        row(index)?.cell(forColumnIndex: column)
    }

    /// Drops the cached elements. For a result being replaced: the elements read
    /// live, so their values need no invalidation, but a shorter result leaves
    /// cached rows that are past its end.
    func invalidate() {
        rowElements.removeAll(keepingCapacity: false)
        cachedColumns = []
    }
}

/// One row of the grid.
@MainActor
final class GridRowAccessibilityElement: NSAccessibilityElement {
    let row: Int
    private let columns: [GridAccessibleColumn]
    private weak var source: (any GridAccessibilitySource)?
    private weak var container: NSView?
    /// Made with the row rather than on demand: a row holds tens of cells, not
    /// millions, and a screen reader that has reached a row is about to read
    /// them.
    private var cells: [GridCellAccessibilityElement] = []

    init(
        row: Int, columns: [GridAccessibleColumn], source: any GridAccessibilitySource,
        container: NSView?
    ) {
        self.row = row
        self.columns = columns
        self.source = source
        self.container = container
        super.init()
        cells = columns.map {
            GridCellAccessibilityElement(row: row, column: $0, source: source, container: container)
        }
    }

    func cell(forColumnIndex index: Int) -> GridCellAccessibilityElement? {
        cells.first { $0.column.index == index }
    }

    override func accessibilityRole() -> NSAccessibility.Role? { .row }

    override func accessibilitySubrole() -> NSAccessibility.Subrole? { .tableRow }

    /// One-based, because it is read out to a person. The status bar counts rows
    /// the same way.
    override func accessibilityLabel() -> String? { "Row \(row + 1)" }

    override func accessibilityIndex() -> Int { row }

    /// Left to right, as they are drawn. Navigation order is not stated
    /// separately: AppKit falls back to this order, which is already the one a
    /// reader moving along the row expects.
    override func accessibilityChildren() -> [Any]? { cells }

    override func isAccessibilitySelected() -> Bool {
        source?.accessibleCursor?.rows.contains(row) ?? false
    }

    override func accessibilityParent() -> Any? { container }

    override func accessibilityFrame() -> NSRect {
        source?.accessibleFrame(row: row, column: nil) ?? .zero
    }
}

/// One cell of the grid: which column it is, and what it holds.
@MainActor
final class GridCellAccessibilityElement: NSAccessibilityElement {
    let row: Int
    let column: GridAccessibleColumn
    private weak var source: (any GridAccessibilitySource)?
    private weak var container: NSView?

    init(
        row: Int, column: GridAccessibleColumn, source: any GridAccessibilitySource,
        container: NSView?
    ) {
        self.row = row
        self.column = column
        self.source = source
        self.container = container
        super.init()
    }

    override func accessibilityRole() -> NSAccessibility.Role? { .cell }

    /// The column's name, which is how a cell is identified out loud: "amount,
    /// 1250". The value carries the rest.
    override func accessibilityLabel() -> String? { column.name }

    override func accessibilityValue() -> Any? {
        source?.accessibleText(row: row, column: column.index)
    }

    override func accessibilityRowIndexRange() -> NSRange {
        NSRange(location: row, length: 1)
    }

    /// The drawn position rather than the index into the result: a screen reader
    /// counting columns is counting the ones it can be told about.
    override func accessibilityColumnIndexRange() -> NSRange {
        NSRange(location: column.position, length: 1)
    }

    override func isAccessibilitySelected() -> Bool {
        guard let cursor = source?.accessibleCursor else { return false }
        return cursor.row == row && cursor.column == column.index
    }

    override func isAccessibilityFocused() -> Bool { isAccessibilitySelected() }

    /// Focusing a cell is how a screen reader navigates, so it moves the same
    /// cursor a click moves and scrolls the cell into view. Without the scroll a
    /// reader could walk a million rows while the grid went on showing the first
    /// forty.
    override func setAccessibilityFocused(_ accessibilityFocused: Bool) {
        guard accessibilityFocused else { return }
        source?.accessibleSelect(row: row, column: column.index)
    }

    override func accessibilityParent() -> Any? {
        source?.accessibleRowElement(row) ?? container
    }

    override func accessibilityFrame() -> NSRect {
        source?.accessibleFrame(row: row, column: column.index) ?? .zero
    }
}

/// The grid, answering for itself.
///
/// Every answer is read out of the renderer — the same widths, the same scroll
/// position, the same hidden columns, the same cursor the drawing uses. Nothing
/// here keeps a copy of anything.
extension GridView: GridAccessibilitySource {
    /// Draft rows included, which is what `totalRows` counts.
    var accessibleRowCount: Int { renderer?.totalRows ?? 0 }

    var accessibleColumns: [GridAccessibleColumn] {
        guard let renderer, let table = renderer.table else { return [] }
        var columns: [GridAccessibleColumn] = []
        for index in table.columns.indices where !renderer.hiddenColumns.contains(index) {
            columns.append(
                GridAccessibleColumn(
                    index: index, position: columns.count, name: table.columns[index].name))
        }
        return columns
    }

    var accessibleVisibleRows: Range<Int> {
        guard let renderer else { return 0..<0 }
        return renderer.visibleRowRange(viewHeight: bounds.height, rowCount: renderer.totalRows)
    }

    var accessibleCursor: GridSelection? { renderer?.selection }

    func accessibleText(row: Int, column: Int) -> String {
        renderer?.cellText(row: row, column: column).0 ?? ""
    }

    func accessibleSelect(row: Int, column: Int) {
        guard let renderer else { return }
        let selection = GridSelection(row: row, column: column)
        apply(selection)
        // The same scroll an arrow key gets. Without it a reader could walk a
        // million rows while the grid went on showing the first forty, which
        // would also leave every frame it asked for empty.
        renderer.scrollToVisible(selection, viewSize: bounds.size)
        needsDisplay = true
    }

    func accessibleFrame(row: Int, column: Int?) -> NSRect {
        guard let renderer, let window else { return .zero }
        let rowHeight = CGFloat(renderer.rowHeight)
        let top =
            CGFloat(renderer.headerHeight) + (CGFloat(row) - CGFloat(renderer.scrollRow))
            * rowHeight
        // Nothing to point at for a row that is scrolled out of sight. Reported
        // as empty rather than as an off-screen rectangle, which a screen reader
        // would draw its box around somewhere over the window's neighbour.
        guard top + rowHeight > CGFloat(renderer.headerHeight), top < bounds.height else {
            return .zero
        }
        var x: CGFloat = 0
        var width = bounds.width
        if let column {
            x = CGFloat(renderer.columnX(column) - renderer.scrollX)
            width = CGFloat(renderer.columnWidth(column))
        }
        // The renderer measures y down from the top — its first row is drawn just
        // below the header — while an `MTKView` is not flipped, so the rectangle
        // has to be turned over before it leaves view coordinates.
        let inView = NSRect(
            x: x, y: bounds.height - top - rowHeight, width: width, height: rowHeight)
        return window.convertToScreen(convert(inView, to: nil))
    }

    func accessibleRowElement(_ row: Int) -> GridRowAccessibilityElement? {
        accessibilityTree.row(row)
    }
}

extension GridAccessibilitySource {
    /// The row element a cell names as its parent. Defaulted to nothing: only a
    /// source that owns a tree can answer it, and a cell whose parent is the
    /// grid itself is still reachable — one level of the hierarchy is flatter
    /// than it should be, which is a worse answer than a wrong one only if it is
    /// never given.
    func accessibleRowElement(_ row: Int) -> GridRowAccessibilityElement? { nil }
}
