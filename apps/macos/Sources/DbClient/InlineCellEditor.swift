import AppKit

/// The text field that floats over a cell while somebody types into it.
///
/// A real `NSTextField` rather than something drawn into the Metal surface. The
/// grid draws glyphs from an atlas and has no caret, no selection, no input
/// method and no undo, and a text editor is the one control on this platform
/// where writing a second version of all four is a mistake with a name.
///
/// It owns when it ends, and tells its owner once. A field that is taken away
/// while it is the first responder ends its own editing session on the way out,
/// so committing has to be idempotent or clicking another cell would stage the
/// same change twice.
final class InlineCellEditor: NSTextField, NSTextFieldDelegate {
    /// Where the cursor should go once the value is kept.
    ///
    /// Carried out of here rather than decided by the grid, because the key that
    /// ended the edit is the only evidence of it and this is where that key
    /// arrives. Return stays put: somebody who has just fixed one value is as
    /// likely to look at it as to move on, and a cursor that walked off the cell
    /// they were reading would be answering for them.
    enum Ending {
        case here
        case next
        case previous
    }

    /// Called with what was typed, once, when Return, Tab or a click elsewhere
    /// ends the edit.
    var onCommit: ((String, Ending) -> Void)?
    /// Called instead when Escape does. The two are separate because they are
    /// different answers: one is a value and the other is "forget I typed that".
    var onCancel: (() -> Void)?

    /// Whether either has already been called. See the type's own note.
    private var finished = false

    init(frame: NSRect, padding: CGFloat) {
        super.init(frame: frame)
        let inset = InsetTextFieldCell(textCell: "")
        inset.padding = padding
        // One line, and one that scrolls rather than wraps: a value longer than
        // its column is a value being scrolled through, not a cell that grew a
        // second line. Where that line sits is `InsetTextFieldCell`'s answer,
        // not this one's.
        inset.usesSingleLineMode = true
        inset.isScrollable = true
        inset.wraps = false
        inset.isEditable = true
        inset.isBordered = false
        inset.drawsBackground = true
        cell = inset

        font = .monospacedSystemFont(ofSize: 12, weight: .regular)
        textColor = Theme.Grid.text.nsColor
        // The grid's own background rather than a lighter one. What marks the
        // cell as live is the caret and the border; a field that also changed
        // colour would read as a second kind of selection.
        backgroundColor = Theme.Grid.background.nsColor
        // Drawn by the layer rather than by a focus ring, which would sit
        // outside the cell and cover the two beside it.
        wantsLayer = true
        layer?.borderWidth = 1
        layer?.borderColor = Theme.Grid.cursor.nsColor.cgColor
        focusRingType = .none
        delegate = self
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not loaded from a nib") }

    /// Return, Tab and a click elsewhere all mean the same thing about the
    /// value: keep it. They differ only in where the cursor goes next.
    func control(
        _ control: NSControl, textView: NSTextView, doCommandBy selector: Selector
    ) -> Bool {
        switch selector {
        case #selector(NSResponder.insertNewline(_:)):
            commit(going: .here)
            return true
        case #selector(NSResponder.insertTab(_:)):
            commit(going: .next)
            return true
        case #selector(NSResponder.insertBacktab(_:)):
            commit(going: .previous)
            return true
        case #selector(NSResponder.cancelOperation(_:)):
            guard !finished else { return true }
            finished = true
            onCancel?()
            return true
        default:
            return false
        }
    }

    /// Focus lost some other way — a click on another cell, on the sidebar, or
    /// on another window. Committed rather than discarded: nothing is sent until
    /// Save, so keeping it costs a mark on a cell that can be reverted, and
    /// discarding it costs whatever was typed.
    func controlTextDidEndEditing(_ obj: Notification) {
        // Nowhere in particular. Whatever took the focus away is where the
        // person is looking, and moving the grid's cursor under it would be this
        // window arguing with the click that just happened.
        commit(going: .here)
    }

    /// Stops it reporting anything at all.
    ///
    /// For the caller that is taking the field away because what it was typed
    /// about is gone. Removing it would otherwise end its editing session, which
    /// arrives here as `controlTextDidEndEditing` and is kept — the right answer
    /// for a click on another cell and the wrong one for a result that has been
    /// replaced underneath it.
    func abandon() {
        finished = true
    }

    private func commit(going ending: Ending) {
        guard !finished else { return }
        finished = true
        onCommit?(stringValue, ending)
    }
}

/// Lays the text out where the grid draws it.
///
/// `NSTextFieldCell` starts its text about two points in from the left; the grid
/// starts its own at `cellPadding`. Without this the characters jump sideways
/// the moment the editor opens, which is the one thing an editor over a drawn
/// cell must not do — the whole illusion is that the cell became typeable.
private final class InsetTextFieldCell: NSTextFieldCell {
    var padding: CGFloat = 6

    /// How far below a row's top edge `GridRenderer` starts a cell's glyphs.
    ///
    /// Copied rather than centred. `NSTextFieldCell` centres its text in the
    /// height it is given, the grid draws its own at a fixed offset, and for a
    /// twenty-point row the two land three points apart — measured off a
    /// screenshot, where the editor's line sat visibly above the rows either
    /// side of it. Two files have to agree about one number; the alternative is
    /// two rules that agree today.
    private let textTop: CGFloat = 3

    /// And how much the field editor puts in front of the first glyph by itself.
    ///
    /// Subtracted from the padding rather than ignored, for the reason the
    /// offset above exists: measured at two points, which is a third of the
    /// grid's own padding and enough to see the line shift as the cell opens.
    private let fieldEditorPadding: CGFloat = 2

    override func drawingRect(forBounds rect: NSRect) -> NSRect {
        let inset = rect.insetBy(dx: padding - fieldEditorPadding, dy: 0)
        return NSRect(
            x: inset.minX, y: inset.minY + textTop,
            width: inset.width, height: inset.height - textTop)
    }

    /// The field editor gets the same rectangle the glyphs would have been drawn
    /// in.
    ///
    /// Overriding `drawingRect` alone is not enough and the screenshot said so:
    /// it places the text a cell draws for itself, and from the moment editing
    /// begins there is a field editor on screen instead, laid out from whatever
    /// rectangle it was handed. Handing it the drawn rect is what makes the two
    /// the same — and it carries the vertical centring across as well, which a
    /// hand-tuned offset would get right until the row height changed.
    override func edit(
        withFrame rect: NSRect, in controlView: NSView, editor: NSText, delegate: Any?,
        event: NSEvent?
    ) {
        super.edit(
            withFrame: drawingRect(forBounds: rect), in: controlView, editor: editor,
            delegate: delegate, event: event)
    }

    override func select(
        withFrame rect: NSRect, in controlView: NSView, editor: NSText, delegate: Any?,
        start: Int, length: Int
    ) {
        super.select(
            withFrame: drawingRect(forBounds: rect), in: controlView, editor: editor,
            delegate: delegate, start: start, length: length)
    }
}
