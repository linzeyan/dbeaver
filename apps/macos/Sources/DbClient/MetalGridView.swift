import MetalKit
import SwiftUI

/// The cell the grid's cursor is on, plus the range it was extended over.
///
/// The cursor stays a single cell even when many rows are selected: the cell
/// inspector, the scroll-into-view and the keyboard all need one point to work
/// from, and a range without a moving end has nothing to extend.
struct GridSelection: Equatable {
    var row: Int
    var column: Int
    /// The row a shift-extended range grew from. Nil for a plain single-cell
    /// selection, so an unshifted arrow key collapses the range by clearing it.
    var anchor: Int?

    /// Selected rows, ordered — the anchor may sit below the cursor.
    var rows: ClosedRange<Int> {
        guard let anchor else { return row...row }
        return min(anchor, row)...max(anchor, row)
    }
}

/// Which column the result is ordered by, by index into the current result.
struct GridSort: Equatable {
    var column: Int
    var descending: Bool
}

/// Bridges the Metal grid into SwiftUI.
///
/// The grid stays AppKit + Metal rather than becoming a SwiftUI `Table`: no
/// view-based table survives a million rows with dynamic columns, and phase 0
/// exists to demonstrate exactly that. SwiftUI owns the chrome; this owns the
/// data surface.
struct MetalGridView: NSViewRepresentable {
    let table: ArrowTable
    /// Changes when the underlying result is replaced, which is the signal to
    /// reset scroll position and redraw.
    let generation: Int
    /// Rows currently in `table`. Nothing here reads it — it is declared so a
    /// result that grew, rather than being replaced, still re-runs
    /// `updateNSView` and redraws. `generation` cannot carry that signal: it
    /// also resets the scroll position, which is the last thing someone who
    /// just asked for more rows wants.
    let rowCount: Int
    /// PostgreSQL's declared type per column name, for the header's type line.
    ///
    /// Left empty by the Query pane on purpose. A statement's columns need not
    /// come from any relation, and matching them by name against the browsed
    /// relation would label a computed `id` with that relation's `id` type — a
    /// type it does not have. The header falls back to the Arrow kind there,
    /// which is always true of whatever arrived.
    var declaredTypes: [String: String] = [:]
    /// Columns not to draw, by index into `table.columns`.
    ///
    /// Empty for the Query pane, and not because it could not be computed there:
    /// a statement's columns were named by the person who wrote it, and hiding
    /// one they typed out would be answering a question they did not ask. A
    /// browse is `SELECT *`, where nobody named anything.
    var hidden: Set<Int> = []
    @Binding var selection: GridSelection?
    /// See `GridView.claimsInitialFocus`.
    var claimsInitialFocus = false
    /// What a screen reader calls this grid. Handed to the AppKit view rather
    /// than applied here with `.accessibilityLabel`: that modifier makes SwiftUI
    /// wrap the representable in an element of its own, which would hide the rows
    /// and cells underneath it — a label on a table with nothing in it.
    var name = "Result grid"
    var sort: GridSort?
    /// Cells holding a change that has not been sent, so the grid can mark them.
    ///
    /// Empty for the Query pane, whose rows belong to no one relation and cannot
    /// be edited. Passed down rather than read from a model, for the reason
    /// `declaredTypes` is: this view draws whatever it is handed and knows
    /// nothing about connections.
    var pending: Set<GridCell> = []
    /// Rows marked to be deleted when the changes are sent. Empty for the Query
    /// pane, for the reason `pending` is.
    var deleted: Set<Int> = []
    /// Rows added and not yet sent, drawn after the last one the result holds.
    /// Empty for the Query pane, for the reason `pending` is.
    var drafts: [DraftRow] = []
    /// Called with a column index when its header is clicked. Nil means this
    /// grid does not sort — the Query pane shows the result of a statement the
    /// user wrote, and appending an ORDER BY to arbitrary SQL is not something
    /// this can do correctly.
    var onSortColumn: ((Int) -> Void)?

    final class Coordinator {
        var renderer: GridRenderer?
        var lastGeneration = -1
        var lastRowCount = -1
        var onSelect: ((GridSelection) -> Void)?
        var onSortColumn: ((Int) -> Void)?
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> GridView {
        guard let device = MTLCreateSystemDefaultDevice() else {
            return GridView(frame: .zero, device: nil)
        }
        // The window is not attached yet, so take the scale from the screen.
        // updateNSView corrects it once the view is in a window.
        let scale = NSScreen.main?.backingScaleFactor ?? 2
        let view = GridView(frame: .zero, device: device)
        view.colorPixelFormat = .bgra8Unorm
        view.clearColor = Theme.Grid.background.mtlClear
        view.isPaused = true
        view.enableSetNeedsDisplay = true
        view.claimsInitialFocus = claimsInitialFocus
        view.accessibilityName = name
        view.onSelect = { [weak coordinator = context.coordinator] hit in
            coordinator?.onSelect?(hit)
        }
        view.onHeaderClick = { [weak coordinator = context.coordinator] column in
            coordinator?.onSortColumn?(column)
        }

        if let renderer = GridRenderer(device: device, scale: scale) {
            renderer.table = table
            renderer.declaredTypes = declaredTypes
            renderer.hiddenColumns = hidden
            view.renderer = renderer
            view.delegate = context.coordinator.makeDelegate(renderer: renderer)
            context.coordinator.renderer = renderer
        }
        return view
    }

    func updateNSView(_ view: GridView, context: Context) {
        guard let renderer = context.coordinator.renderer else { return }
        renderer.table = table
        renderer.declaredTypes = declaredTypes
        renderer.hiddenColumns = hidden
        // Re-captured each update so the closure writes through the current
        // binding rather than the one that existed when the view was made.
        context.coordinator.onSelect = { selection = $0 }
        context.coordinator.onSortColumn = onSortColumn
        view.sortsOnHeaderClick = onSortColumn != nil
        renderer.sort = sort
        renderer.pending = pending
        renderer.deleted = deleted
        renderer.drafts = drafts

        if context.coordinator.lastGeneration != generation {
            context.coordinator.lastGeneration = generation
            // A new result starts at the top; keeping the old offset would show
            // an arbitrary window of unrelated data.
            renderer.scrollRow = 0
            renderer.scrollX = 0
        }
        // A page arriving changes how many rows there are without replacing the
        // result, and that is exactly the case a screen reader cannot see: it
        // asked for the count once.
        if context.coordinator.lastRowCount != rowCount + drafts.count {
            context.coordinator.lastRowCount = rowCount + drafts.count
            view.resultDidChange()
        }
        // The model owns the selection, including the one it sets when a result
        // arrives, so the renderer follows the binding rather than being reset
        // here — resetting would discard that initial selection every time.
        if renderer.selection != selection { renderer.selection = selection }
        view.needsDisplay = true
    }
}

extension MetalGridView.Coordinator {
    /// Retains the delegate, which MTKView holds weakly.
    func makeDelegate(renderer: GridRenderer) -> MTKViewDelegate {
        let d = GridDrawDelegate(renderer: renderer)
        retainedDelegate = d
        return d
    }
}

private var retainedDelegateKey: UInt8 = 0
extension MetalGridView.Coordinator {
    var retainedDelegate: MTKViewDelegate? {
        get { objc_getAssociatedObject(self, &retainedDelegateKey) as? MTKViewDelegate }
        set {
            objc_setAssociatedObject(
                self, &retainedDelegateKey, newValue, .OBJC_ASSOCIATION_RETAIN)
        }
    }
}

/// Minimal delegate: the benchmark harness has its own, which also drives the
/// scripted scroll and frame statistics.
final class GridDrawDelegate: NSObject, MTKViewDelegate {
    private let renderer: GridRenderer

    init(renderer: GridRenderer) {
        self.renderer = renderer
    }

    func mtkView(_ view: MTKView, drawableSizeWillChange size: CGSize) {}

    func draw(in view: MTKView) {
        renderer.draw(in: view)
    }
}
