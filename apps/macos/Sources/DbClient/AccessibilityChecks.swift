import AppKit
import MetalKit

/// Executable checks for the grid's accessibility tree, run by
/// `--verify-accessibility`.
///
/// A screen reader is the one reader of this application nobody on the team is,
/// so nothing here can be checked by looking at it — the tree is invisible in
/// every screenshot the capture tool takes, and a wrong answer looks exactly like
/// a right one. What can be wrong is arithmetic: a row numbered from the wrong
/// end, a value read out of the column beside the one being announced, a cursor
/// the reader moves that the grid does not follow, a million rows built as a
/// million objects.
///
/// Run against a stub source rather than a live grid, which is why
/// `GridAccessibilitySource` is a protocol: a `GridView` needs a Metal device and
/// its rows need a database, and none of the arithmetic above needs either. What
/// the stub cannot cover is the renderer's spelling of a cell — that needs an
/// Arrow table, which needs a server — so this file checks the plumbing and
/// `GridRenderer.cellText` stays the single place the words are decided.
@MainActor
enum AccessibilityChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkEveryRowIncludingTheDraftsIsReachable()
        checkARowIsNumberedTheWayAPersonCountsRows()
        checkACellIsNamedByItsColumnAndValuedByItsCell()
        checkACellReadsItsValueEveryTimeItIsAsked()
        checkAHiddenColumnIsNotOfferedAtAll()
        checkTheColumnNumberIsTheOneOnScreen()
        checkTheSelectedRowsAreTheRangeTheCursorCovers()
        checkFocusingACellMovesTheGridsOwnCursor()
        checkAMillionRowsAreHandedOverASliceAtATime()
        checkSelectingEveryRowStaysCheapToDescribe()
        checkWalkingAWholeResultDoesNotGrowWithoutBound()
        checkADifferentSetOfColumnsThrowsAwayTheOldRows()
        checkTheGridIsNotFlippedSoAFrameHasToTurnOver()
        checkAClickNearTheTopOfTheViewLandsNearTheTopOfTheGrid()
        if failures == 0 {
            fputs("accessibility: all checks passed\n", stderr)
        } else {
            fputs("accessibility: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - What there is to read

    /// A draft row is on screen, so it is in the tree.
    ///
    /// The row a user has just added is the one they are working on, and it is
    /// drawn past the last row the result holds. A tree that stopped at the
    /// fetched rows would put it out of reach of the only reader who cannot see
    /// it there.
    private static func checkEveryRowIncludingTheDraftsIsReachable() {
        let grid = StubGrid(rows: 4, drafts: 2)
        let tree = GridAccessibilityTree(source: grid, container: nil)
        expect(tree.rowCount, 6, "four fetched rows and two drafts are six rows")
        expect(tree.row(5) != nil, true, "the last draft has an element")
        expect(tree.row(6) == nil, true, "and there is nothing past it")
        expect(tree.row(-1) == nil, true, "nor before the first")
    }

    /// Rows are announced from one, the way the status bar counts them.
    private static func checkARowIsNumberedTheWayAPersonCountsRows() {
        let grid = StubGrid(rows: 3)
        let tree = GridAccessibilityTree(source: grid, container: nil)
        guard let first = tree.row(0), let last = tree.row(2) else {
            fail("the first and last rows have elements")
            return
        }
        expect(first.accessibilityLabel(), "Row 1", "the first row is row one")
        expect(last.accessibilityLabel(), "Row 3", "the third row is row three")
        // The index stays zero-based: it is what a client uses to ask for a
        // slice, not something read out.
        expect(first.accessibilityIndex(), 0, "the index is the one the API means")
        expect(first.accessibilityRole(), .row, "a row is a row")
    }

    /// The column names the cell, the value carries the data. Read in the order
    /// the columns are drawn — a value announced under the neighbouring column's
    /// name is the failure that looks like working software.
    private static func checkACellIsNamedByItsColumnAndValuedByItsCell() {
        let grid = StubGrid(rows: 2)
        let tree = GridAccessibilityTree(source: grid, container: nil)
        guard let row = tree.row(1),
            let cells = row.accessibilityChildren() as? [GridCellAccessibilityElement]
        else {
            fail("the second row has cells")
            return
        }
        expect(cells.count, 3, "one cell per column")
        let names = cells.compactMap { $0.accessibilityLabel() }
        expect(names, ["id", "name", "amount"], "each cell is named by its column")
        let values = cells.compactMap { $0.accessibilityValue() as? String }
        expect(values, ["r1c0", "r1c1", "r1c2"], "and holds its own row's values")
    }

    /// A cell holds no copy of anything.
    ///
    /// The rows arrive in pages and a staged edit changes a value in place, both
    /// without the cell it happened in becoming a different cell. An element that
    /// had cached its value would go on reading out what the grid used to show.
    private static func checkACellReadsItsValueEveryTimeItIsAsked() {
        let grid = StubGrid(rows: 2)
        let tree = GridAccessibilityTree(source: grid, container: nil)
        guard let cell = tree.cell(row: 0, column: 1) else {
            fail("the cell has an element")
            return
        }
        expect(cell.accessibilityValue() as? String, "r0c1", "the value it was made over")
        grid.staged["0.1"] = "edited"
        expect(cell.accessibilityValue() as? String, "edited", "and the value it has now")
    }

    // MARK: - Columns the grid is not drawing

    /// A hidden column is left out rather than announced as empty.
    ///
    /// The tree describes what is on screen. An empty-column setting that hid a
    /// column from the grid and left it in the tree would give a screen reader a
    /// column the sighted reader is not looking at.
    private static func checkAHiddenColumnIsNotOfferedAtAll() {
        let grid = StubGrid(rows: 2, hidden: [1])
        let tree = GridAccessibilityTree(source: grid, container: nil)
        expect(tree.columns.map(\.name), ["id", "amount"], "the hidden column is not a column")
        expect(tree.cell(row: 0, column: 1) == nil, true, "and has no cell to be read")
        guard let cell = tree.cell(row: 0, column: 2) else {
            fail("the column past the hidden one still has a cell")
            return
        }
        expect(cell.accessibilityValue() as? String, "r0c2", "reading its own value")
    }

    /// A column is numbered by where it is drawn, not by where it is in the
    /// result: a reader counting columns is counting the ones they can be told
    /// about. With the second column hidden, the third is column two.
    private static func checkTheColumnNumberIsTheOneOnScreen() {
        let grid = StubGrid(rows: 1, hidden: [1])
        let tree = GridAccessibilityTree(source: grid, container: nil)
        guard let cell = tree.cell(row: 0, column: 2) else {
            fail("the third column has a cell")
            return
        }
        expect(cell.accessibilityColumnIndexRange().location, 1, "drawn second, announced second")
        expect(cell.accessibilityRowIndexRange().location, 0, "in the first row")
    }

    // MARK: - The cursor

    /// A shift-extended selection is a range of rows, and the tree reports all of
    /// them: the grid draws the band that wide, and Copy takes that many rows.
    private static func checkTheSelectedRowsAreTheRangeTheCursorCovers() {
        let grid = StubGrid(rows: 10)
        grid.cursor = GridSelection(row: 5, column: 1, anchor: 3)
        let tree = GridAccessibilityTree(source: grid, container: nil)
        expect(tree.selectedRowCount, 3, "three rows are selected")
        let selected = tree.boundedSelectedRows().compactMap { $0 as? GridRowAccessibilityElement }
        expect(selected.map(\.row), [3, 4, 5], "the anchor's row through the cursor's")
        expect(selected.allSatisfy { $0.isAccessibilitySelected() }, true, "each says so itself")
        expect(tree.row(2)?.isAccessibilitySelected(), false, "the row above does not")
        // The cursor is one cell inside that band — the cell the inspector reads
        // and the keyboard moves from.
        expect(tree.focusedCell()?.row, 5, "the focused cell is on the cursor's row")
        expect(tree.focusedCell()?.column.index, 1, "in the cursor's column")
    }

    /// Focusing a cell moves the grid's own cursor.
    ///
    /// This is how a screen reader navigates a table, and it has to move the same
    /// cursor the keyboard and the mouse move. Two cursors would let the cell
    /// inspector below the grid describe a cell nobody is standing on.
    private static func checkFocusingACellMovesTheGridsOwnCursor() {
        let grid = StubGrid(rows: 20)
        let tree = GridAccessibilityTree(source: grid, container: nil)
        tree.cell(row: 12, column: 2)?.setAccessibilityFocused(true)
        expect(grid.selected.count, 1, "the grid was asked to move once")
        expect(grid.selected.last?.row, 12, "to that row")
        expect(grid.selected.last?.column, 2, "and that column")

        // Unfocusing is not a request to move anywhere: the cursor stays where
        // the reader left it.
        tree.cell(row: 12, column: 2)?.setAccessibilityFocused(false)
        expect(grid.selected.count, 1, "and losing focus moves nothing")
    }

    // MARK: - A million rows

    /// The whole point of the lazy tree: a slice is a slice.
    ///
    /// A browse holds up to a million rows. Asking for the forty a reader is about
    /// to read must produce forty objects — not a million, which is the stall this
    /// file exists to avoid.
    private static func checkAMillionRowsAreHandedOverASliceAtATime() {
        let grid = StubGrid(rows: 1_000_000)
        let tree = GridAccessibilityTree(source: grid, container: nil)
        expect(tree.rowCount, 1_000_000, "the count is the honest one")

        let window = tree.rows(from: 500_000, maxCount: 40)
        expect(window.count, 40, "forty rows came back")
        expect(tree.elementsMade, 40, "and forty objects were made")
        expect(
            (window.first as? GridRowAccessibilityElement)?.row, 500_000,
            "starting where it was asked to")

        // The end of the result is where an off-by-one lands: forty asked for,
        // ten left.
        expect(
            tree.rows(from: 999_990, maxCount: 40).count, 10, "the last page is short, not wrong")
        expect(tree.rows(from: 1_000_000, maxCount: 40).count, 0, "and past the end there is none")
    }

    /// ⌘A selects every row there is, and describing that must stay cheap.
    ///
    /// The count comes from the range, and the array of selected rows is bounded
    /// the way every other whole-array answer is. Without this, one press of ⌘A
    /// over a million-row browse would turn the next accessibility question into
    /// a million-object build — a hang, in the code that exists to describe the
    /// window it is hanging.
    private static func checkSelectingEveryRowStaysCheapToDescribe() {
        let grid = StubGrid(rows: 1_000_000)
        grid.cursor = GridSelection(row: 999_999, column: 0, anchor: 0)
        let tree = GridAccessibilityTree(source: grid, container: nil)
        expect(tree.selectedRowCount, 1_000_000, "every row is selected")
        expect(tree.elementsMade, 0, "and counting them made no elements")

        let slice = tree.selectedRows(from: 100, maxCount: 20)
        expect(slice.count, 20, "a slice of the selection is a slice")
        expect(
            (slice.first as? GridRowAccessibilityElement)?.row, 100,
            "offset from the start of the selection")
        expect(
            tree.boundedSelectedRows().count, GridAccessibilityTree.cacheBound,
            "and asking for all of them stops at the bound")
    }

    /// Reading a long result does not accumulate a row object per row.
    ///
    /// The cache is bounded and dropped whole when it fills. Asserted because the
    /// failure is invisible in use: every answer stays correct while the memory
    /// climbs for as long as the reader keeps reading.
    private static func checkWalkingAWholeResultDoesNotGrowWithoutBound() {
        let grid = StubGrid(rows: 200_000)
        let tree = GridAccessibilityTree(source: grid, container: nil)
        for row in stride(from: 0, to: 200_000, by: 10) { _ = tree.row(row) }
        expect(
            tree.elementsMade <= GridAccessibilityTree.cacheBound, true,
            "the cache stayed inside its bound")
        // Still correct afterwards: dropping the cache must cost a rebuild, not
        // an answer.
        expect(
            tree.row(199_999)?.accessibilityLabel(), "Row 200000", "and still answers for any row")
    }

    /// A result with different columns is a different table.
    ///
    /// Rows cached against the previous schema would describe this one's cells
    /// under the previous one's names — the same failure as reading the wrong
    /// column, arriving one query later.
    private static func checkADifferentSetOfColumnsThrowsAwayTheOldRows() {
        let grid = StubGrid(rows: 3)
        let tree = GridAccessibilityTree(source: grid, container: nil)
        let before = tree.row(0)
        expect(before?.accessibilityChildren()?.count, 3, "three columns to begin with")

        grid.names = ["ok"]
        guard let after = tree.row(0) else {
            fail("the row still has an element")
            return
        }
        expect(after === before, false, "the row was made again")
        expect(
            (after.accessibilityChildren()?.first as? GridCellAccessibilityElement)?
                .accessibilityLabel(), "ok",
            "against the columns the result has now")
    }

    // MARK: - Coordinates

    /// A click near the top of the view is near the top of the grid.
    ///
    /// The other direction of the same contract the frames rely on, and it lives
    /// here because it is one fact about the view rather than two: the renderer
    /// measures y down from the top, so a pointer y measured up from the bottom
    /// has to be turned over before the renderer is asked anything about it. It
    /// was not, and every pointer answer was mirrored — a click on the first row
    /// selected one counted from the other end, which is the defect this check
    /// exists to keep from coming back.
    private static func checkAClickNearTheTopOfTheViewLandsNearTheTopOfTheGrid() {
        let height: CGFloat = 300
        let atTop = GridView.rendererPoint(
            of: CGPoint(x: 40, y: height - 5), viewHeight: height)
        expect(atTop.y, 5, "five points below the top edge is five points into the header")
        expect(atTop.x, 40, "and x is left alone")

        let atBottom = GridView.rendererPoint(of: CGPoint(x: 40, y: 2), viewHeight: height)
        expect(atBottom.y, height - 2, "two points above the bottom edge is the far end")

        // And the consequence, worked out the way `GridRenderer.cell(at:)` works it
        // out. A renderer needs a Metal device, so this states its two constants
        // rather than reading them; if they move, the arithmetic here stops
        // matching the grid and this comment is the reason why.
        let headerHeight: CGFloat = 32
        let rowHeight: CGFloat = 20
        func row(clickedAt viewY: CGFloat) -> Int {
            let y = GridView.rendererPoint(of: CGPoint(x: 40, y: viewY), viewHeight: height).y
            return Int((y - headerHeight) / rowHeight)
        }
        expect(row(clickedAt: height - headerHeight - 5), 0, "the first row is the top one")
        expect(row(clickedAt: height - headerHeight - 25), 1, "and the second is below it")
        expect(row(clickedAt: 5), 13, "while the bottom of the view is the far end of the page")
    }

    /// The contract `accessibleFrame` turns a rectangle over for.
    ///
    /// The renderer measures y down from the top — the header band is drawn at
    /// y = 0 — while the view it draws into is not flipped, so a rectangle built
    /// from renderer geometry is upside down until it is converted. Asserted
    /// rather than assumed: if `MTKView` ever became flipped, every frame this
    /// reports would be mirrored about the middle of the grid, and a screen
    /// reader's box would sit on a different row than the one being read.
    private static func checkTheGridIsNotFlippedSoAFrameHasToTurnOver() {
        let view = GridView(frame: NSRect(x: 0, y: 0, width: 400, height: 300), device: nil)
        expect(view.isFlipped, false, "the grid's y axis runs up from the bottom")
        // With no renderer there is no geometry, and nothing is claimed about
        // where a cell is. A frame invented here would be a box drawn over the
        // wrong part of the screen.
        expect(view.accessibleFrame(row: 0, column: 0), .zero, "and no renderer means no frame")
        expect(view.accessibilityRole(), .table, "the grid presents itself as a table")
        expect(view.accessibilityLabel(), "Result grid", "under the name it was given")
    }

    // MARK: - Harness

    /// A grid without a grid: the counts, the columns, the values and the cursor,
    /// with nothing drawn and nothing fetched.
    @MainActor
    private final class StubGrid: GridAccessibilitySource {
        var fetched: Int
        var drafts: Int
        var names: [String]
        var hidden: Set<Int>
        var cursor: GridSelection?
        /// Values overridden since the stub was made, keyed `"row.column"`, for
        /// the page-arrives-later case.
        var staged: [String: String] = [:]
        /// Every cell the tree asked to be selected, in order.
        var selected: [GridSelection] = []

        init(rows: Int, drafts: Int = 0, hidden: Set<Int> = [], names: [String]? = nil) {
            self.fetched = rows
            self.drafts = drafts
            self.hidden = hidden
            self.names = names ?? ["id", "name", "amount"]
        }

        var accessibleRowCount: Int { fetched + drafts }

        var accessibleColumns: [GridAccessibleColumn] {
            var columns: [GridAccessibleColumn] = []
            for index in names.indices where !hidden.contains(index) {
                columns.append(
                    GridAccessibleColumn(
                        index: index, position: columns.count, name: names[index]))
            }
            return columns
        }

        var accessibleVisibleRows: Range<Int> { 0..<min(accessibleRowCount, 40) }

        var accessibleCursor: GridSelection? { cursor }

        func accessibleText(row: Int, column: Int) -> String {
            staged["\(row).\(column)"] ?? "r\(row)c\(column)"
        }

        func accessibleSelect(row: Int, column: Int) {
            let selection = GridSelection(row: row, column: column)
            selected.append(selection)
            cursor = selection
        }

        func accessibleFrame(row: Int, column: Int?) -> NSRect { .zero }
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("accessibility FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }

    private static func fail(_ what: String) {
        failures += 1
        fputs("accessibility FAIL: \(what)\n", stderr)
    }
}
