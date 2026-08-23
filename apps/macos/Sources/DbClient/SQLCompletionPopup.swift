import AppKit

/// The list of names the editor shows under the caret.
///
/// A window of its own rather than a view in the editor's hierarchy, because it
/// has to be able to hang below the last line of a scrolled text view and over
/// whatever is beneath the pane. It is a `nonactivatingPanel` and never becomes
/// key: the caret has to keep blinking and the keys have to keep arriving at the
/// text view while this is up, which is also why every key it responds to is
/// intercepted in `SQLEditor.Coordinator` rather than handled here.
///
/// AppKit has a completion popup of its own, reached through
/// `NSTextView.complete(_:)`. It is not used, for one reason that rules it out:
/// its delegate hands back the list synchronously, and this list comes from the
/// far side of a socket. Answering that call would mean blocking the main thread
/// on a metadata read the first time a connection completes anything.
final class CompletionPopup: NSObject {
    /// What the list is showing, in order.
    private(set) var offers: [SQLCompletion.Offer] = []

    /// The characters accepting an offer replaces, as the core counted them.
    /// Carried with the list because the buffer may have moved on by the time an
    /// answer arrives, and the caller checks this against what it asked about.
    private(set) var replacing: Range<Int> = 0..<0

    var isVisible: Bool { panel.isVisible }

    var selectedOffer: SQLCompletion.Offer? {
        let row = table.selectedRow
        return offers.indices.contains(row) ? offers[row] : nil
    }

    /// Called when a row is clicked, since a list that can only be driven from
    /// the keyboard is a list that ignores half the ways people use a mouse.
    var onAccept: (() -> Void)?

    /// The editor's point size, which the editor keeps current. The list rides
    /// a point behind the text it completes and the detail a point behind that
    /// — the same steps the fixed 12 and 11 used to encode against a 13pt
    /// editor — so resizing the editor keeps the popup in proportion rather
    /// than leaving it at yesterday's size.
    var fontSize: CGFloat = 13

    private let panel: NSPanel
    private let table = NSTableView()
    private let scroll = NSScrollView()

    /// How the window this list lives in is found from outside. See `init`.
    static let identifier = NSUserInterfaceItemIdentifier("sql-completion")

    /// Rows shown before the list starts scrolling. Ten is about a third of the
    /// editor's height at the default window size — enough to choose from, and
    /// short enough that it does not become the thing on screen.
    private static let visibleRows = 10

    private var labelFont: NSFont {
        .monospacedSystemFont(ofSize: fontSize - 1, weight: .regular)
    }
    private var detailFont: NSFont { .systemFont(ofSize: fontSize - 2) }

    /// The label's size plus the ten points of breathing room the fixed 22
    /// gave a 12pt label, so a resized list keeps its density.
    private var rowHeight: CGFloat { fontSize + 9 }

    override init() {
        panel = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: 320, height: 120),
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered, defer: true)
        super.init()

        // Named so that something outside can find it. `--complete` reports
        // where the list landed, which is the only automated evidence there is
        // that it appeared at all: a popup is verified by looking at it, and a
        // capture cannot press the key that opens one.
        panel.identifier = Self.identifier
        panel.isFloatingPanel = true
        panel.level = .popUpMenu
        panel.hidesOnDeactivate = true
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = true
        // Never the key window. The editor keeps the caret and the keys while
        // this is up; the panel is a picture of a decision being made in it.
        panel.becomesKeyOnlyIfNeeded = true

        let container = NSView()
        container.wantsLayer = true
        container.layer?.backgroundColor = Theme.surface.nsColor.cgColor
        container.layer?.cornerRadius = Theme.Radius.card
        container.layer?.borderWidth = 1
        container.layer?.borderColor = Theme.border.opacity(0.5).nsColor.cgColor
        container.layer?.masksToBounds = true

        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("offer"))
        column.resizingMask = .autoresizingMask
        table.addTableColumn(column)
        table.headerView = nil
        table.rowHeight = rowHeight
        table.intercellSpacing = NSSize(width: 0, height: 0)
        table.backgroundColor = .clear
        table.selectionHighlightStyle = .regular
        table.allowsEmptySelection = false
        table.allowsMultipleSelection = false
        table.dataSource = self
        table.delegate = self
        table.target = self
        table.action = #selector(rowClicked)
        table.style = .plain

        scroll.documentView = table
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.drawsBackground = false
        scroll.borderType = .noBorder
        scroll.automaticallyAdjustsContentInsets = false

        container.addSubview(scroll)
        panel.contentView = container
    }

    deinit {
        // A panel still parented to a window that is closing keeps it alive, and
        // a child window left behind outlives the editor it was describing.
        panel.orderOut(nil)
    }

    // MARK: - Showing

    /// Puts `offers` on screen under `caret`, which is in screen coordinates.
    ///
    /// Under it where there is room and over it where there is not, measured
    /// against the screen the caret is on: an editor near the bottom of the
    /// display is the ordinary case for a script being typed downwards, and a
    /// list that hangs off the edge of the screen shows its first row and hides
    /// the rest.
    func show(
        _ offers: [SQLCompletion.Offer], replacing: Range<Int>, under caret: NSRect,
        in window: NSWindow
    ) {
        guard !offers.isEmpty else {
            hide()
            return
        }
        self.offers = offers
        self.replacing = replacing
        // Re-taken on every show rather than only in `init`, because the
        // editor may have been resized since the last list went up.
        table.rowHeight = rowHeight
        table.reloadData()
        table.selectRowIndexes([0], byExtendingSelection: false)
        table.scrollRowToVisible(0)

        let size = fittingSize(for: offers)
        let screen = window.screen ?? NSScreen.main
        let below = NSPoint(x: caret.minX, y: caret.minY - size.height - 4)
        let above = NSPoint(x: caret.minX, y: caret.maxY + 4)
        var origin = below
        if let visible = screen?.visibleFrame {
            if below.y < visible.minY { origin = above }
            origin.x = min(origin.x, visible.maxX - size.width - 8)
            origin.x = max(origin.x, visible.minX + 8)
        }

        panel.setFrame(NSRect(origin: origin, size: size), display: true)
        scroll.frame = panel.contentView?.bounds ?? NSRect(origin: .zero, size: size)
        scroll.autoresizingMask = [.width, .height]

        if panel.parent !== window {
            panel.parent?.removeChildWindow(panel)
            // A child window so that dragging the editor's window carries the
            // list with it rather than leaving it behind over the desktop.
            window.addChildWindow(panel, ordered: .above)
        }
        panel.orderFront(nil)
    }

    func hide() {
        guard panel.isVisible else { return }
        panel.parent?.removeChildWindow(panel)
        panel.orderOut(nil)
        offers = []
    }

    /// Moves the selection by `delta` rows, stopping at either end.
    ///
    /// Stopping rather than wrapping: the arrow keys are how the caret moves
    /// everywhere else in this editor, and a list that jumps from the last row
    /// to the first has taken a key the user pressed to go down and gone up
    /// with it.
    func move(by delta: Int) {
        guard !offers.isEmpty else { return }
        let next = min(max(table.selectedRow + delta, 0), offers.count - 1)
        table.selectRowIndexes([next], byExtendingSelection: false)
        table.scrollRowToVisible(next)
    }

    @objc private func rowClicked() {
        guard table.clickedRow >= 0 else { return }
        onAccept?()
    }

    /// Wide enough for the longest row, within reason.
    ///
    /// Measured rather than fixed, because a list of column names is narrow and
    /// a list of qualified relation names is not, and a fixed width either
    /// truncates the second or wastes half the screen on the first.
    private func fittingSize(for offers: [SQLCompletion.Offer]) -> NSSize {
        var widest: CGFloat = 0
        // Only the first screenful is measured. A thousand-column relation would
        // otherwise pay for a text measurement per name on every keystroke, to
        // decide the width of a list showing ten of them.
        for offer in offers.prefix(Self.visibleRows * 3) {
            let l = (offer.label as NSString).size(withAttributes: [.font: labelFont]).width
            let d = (offer.detail as NSString).size(withAttributes: [.font: detailFont]).width
            widest = max(widest, l + d)
        }
        let width = min(max(widest + CompletionRow.padding, 240), 560)
        let rows = CGFloat(min(offers.count, Self.visibleRows))
        return NSSize(width: width, height: rows * rowHeight)
    }
}

extension CompletionPopup: NSTableViewDataSource, NSTableViewDelegate {
    func numberOfRows(in tableView: NSTableView) -> Int { offers.count }

    func tableView(_ tableView: NSTableView, viewFor column: NSTableColumn?, row: Int) -> NSView? {
        let view =
            tableView.makeView(withIdentifier: CompletionRow.identifier, owner: self)
            as? CompletionRow ?? CompletionRow()
        view.identifier = CompletionRow.identifier
        view.show(offers[row], label: labelFont, detail: detailFont)
        return view
    }

    func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
        SelectionRow()
    }
}

/// One line of the list: a glyph, the name, and what it is.
private final class CompletionRow: NSView {
    static let identifier = NSUserInterfaceItemIdentifier("CompletionRow")
    /// Everything horizontal that is not the two strings.
    static let padding: CGFloat = 48

    private let glyph = NSImageView()
    private let label = NSTextField(labelWithString: "")
    private let detail = NSTextField(labelWithString: "")

    init() {
        super.init(frame: .zero)
        glyph.imageScaling = .scaleProportionallyDown
        glyph.contentTintColor = Theme.textTertiary.nsColor
        label.textColor = Theme.text.nsColor
        detail.textColor = Theme.textTertiary.nsColor
        detail.alignment = .right
        detail.lineBreakMode = .byTruncatingTail
        label.lineBreakMode = .byTruncatingMiddle
        for view in [glyph, label, detail] { addSubview(view) }
    }

    @available(*, unavailable) required init?(coder: NSCoder) { nil }

    /// The fonts come with each show rather than living here, because the row
    /// is recycled and the popup is the one that knows the editor's size.
    func show(_ offer: SQLCompletion.Offer, label labelFont: NSFont, detail detailFont: NSFont) {
        glyph.image = NSImage(
            systemSymbolName: offer.kind.symbol, accessibilityDescription: offer.kind.rawValue)
        label.font = labelFont
        label.stringValue = offer.label
        detail.font = detailFont
        detail.stringValue = offer.detail
        needsLayout = true
    }

    /// Laid out by hand. Three subviews in a row that is created and recycled on
    /// every keystroke is the case where constraint solving costs more than the
    /// arithmetic it replaces.
    override func layout() {
        let inset: CGFloat = 8
        let glyphWidth: CGFloat = 16
        glyph.frame = NSRect(
            x: inset, y: (bounds.height - glyphWidth) / 2, width: glyphWidth, height: glyphWidth)
        let textLeft = inset + glyphWidth + 6
        let detailWidth = min(
            detail.intrinsicContentSize.width, max(bounds.width - textLeft - 60, 0))
        let labelWidth = max(bounds.width - textLeft - detailWidth - inset - 8, 0)
        label.frame = NSRect(
            x: textLeft, y: (bounds.height - label.intrinsicContentSize.height) / 2,
            width: labelWidth, height: label.intrinsicContentSize.height)
        detail.frame = NSRect(
            x: bounds.width - detailWidth - inset,
            y: (bounds.height - detail.intrinsicContentSize.height) / 2,
            width: detailWidth, height: detail.intrinsicContentSize.height)
    }
}

/// The selected row, in the accent the rest of the window selects with.
///
/// AppKit's own selection is the system blue and draws a rounded capsule inset
/// from the edges, neither of which is what anything else here looks like.
private final class SelectionRow: NSTableRowView {
    override func drawSelection(in dirtyRect: NSRect) {
        guard selectionHighlightStyle != .none else { return }
        Theme.accent.opacity(0.35).nsColor.setFill()
        bounds.fill()
    }
}
