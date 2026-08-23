import AppKit
import SwiftUI

/// The Query tab's editor: an `NSTextView` wearing a SwiftUI face, so that SQL
/// can be coloured.
///
/// `TextEditor` cannot be. It renders a plain `String` and offers no way to
/// attribute it, so a keyword, a string literal and a comment all reach the
/// screen as the same grey — which is precisely where a misplaced quote hides.
/// Every other surface in this window is typed and toned; the one place a user
/// writes SQL was the exception.
///
/// The swap has to carry over everything the SwiftUI editor already did, and one
/// thing in particular. The selection binding is what makes ⌘R mean "this
/// statement" and what lets `pointAtSyntaxError` put the caret on the token the
/// server complained about; break the round trip and ⌘R runs the wrong statement
/// of the script while the error caret lands nowhere. So the caret crosses this
/// boundary twice — `NSRange` out, `TextSelection` in — and both directions are
/// guarded against writing back what they were just handed.
struct SQLEditor: NSViewRepresentable {
    @Binding var text: String
    @Binding var selection: TextSelection?

    /// The connection's scheme, which is how the core picks the dialect this
    /// buffer is read in. Passed in rather than looked up here because the view
    /// is handed the two bindings it needs and nothing else, and a database is
    /// not a fact about a text view.
    let scheme: String

    /// The point size the editor draws at. A number rather than the
    /// `Preferences` object it lives in, for the reason `scheme` is a string:
    /// the view is handed what it needs and nothing else, and reading the
    /// preference here would also hide from SwiftUI which property the pane
    /// depends on.
    let fontSize: Int

    /// The typing habits the key handling reads — indent, tabs and their
    /// widths. One value, for the reason `fontSize` is a number: the rules stay
    /// checkable as data, and the pane's dependencies stay visible to SwiftUI.
    let typing: EditorTyping.Rules

    /// Asks the core what could be typed at `caret`, and calls back on the main
    /// actor when the answer arrives.
    ///
    /// A closure rather than a connection, for the reason `scheme` is a string:
    /// this view is handed what it needs and nothing else. It is asynchronous
    /// because the first answer for a connection is a metadata read on the far
    /// side of a socket — a text view that waited for one would stop taking
    /// keystrokes while it did.
    let offers:
        (_ text: String, _ caret: Int, _ then: @escaping (SQLCompletion.Answer) -> Void)
            -> Void

    /// True while `.focused($focus, equals: .editor)` in `QueryPane` points
    /// here. SwiftUI cannot make an arbitrary `NSView` first responder on its
    /// own, so switching to the Query tab would otherwise leave the editor
    /// needing a click before it accepted typing.
    @Environment(\.isFocused) private var isFocused

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    func makeNSView(context: Context) -> NSScrollView {
        let coordinator = context.coordinator

        // TextKit 1, built by hand rather than left to `NSTextView(frame:)`,
        // which gives TextKit 2 on this OS. The whole highlighting strategy
        // below rests on `NSLayoutManager`'s temporary attributes, and reaching
        // for `.layoutManager` later would convert the stack anyway — silently,
        // and at whatever moment the first colour was applied.
        let storage = NSTextStorage()
        let layout = NSLayoutManager()
        let container = NSTextContainer(
            size: CGSize(width: 0, height: CGFloat.greatestFiniteMagnitude))
        container.widthTracksTextView = true
        storage.addLayoutManager(layout)
        layout.addTextContainer(container)

        let textView = EditorTextView(frame: .zero, textContainer: container)
        textView.delegate = coordinator
        textView.editor = coordinator
        textView.isRichText = false
        textView.allowsUndo = true
        textView.isVerticallyResizable = true
        textView.isHorizontallyResizable = false
        textView.autoresizingMask = [.width]
        textView.minSize = .zero
        textView.maxSize = CGSize(
            width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        textView.textContainerInset = .zero
        textView.drawsBackground = true
        textView.backgroundColor = Theme.background.nsColor
        textView.insertionPointColor = Theme.Editor.caret.nsColor
        textView.selectedTextAttributes = [
            .backgroundColor: Theme.Editor.selection.nsColor
        ]
        textView.textColor = Theme.Editor.text.nsColor

        // Every one of these is on by default in an `NSTextView`, and every one
        // of them corrupts SQL. Smart quotes are the dangerous one: it replaces
        // the `'` a user types with `’`, which the server does not accept as a
        // string delimiter and which is indistinguishable at 13pt from the one
        // that would have worked.
        textView.isAutomaticQuoteSubstitutionEnabled = false
        textView.isAutomaticDashSubstitutionEnabled = false
        textView.isAutomaticTextReplacementEnabled = false
        textView.isAutomaticSpellingCorrectionEnabled = false
        textView.isContinuousSpellCheckingEnabled = false
        textView.isGrammarCheckingEnabled = false
        textView.isAutomaticLinkDetectionEnabled = false
        textView.isAutomaticDataDetectionEnabled = false

        // The system find bar, not a search field drawn here. AppKit's bar
        // brings the whole behaviour with it — case-insensitive contains, the
        // match count, wrap-around, incremental highlighting — and presents
        // itself inside the enclosing scroll view, so nothing in this layout
        // has to make room for it. The Edit menu's four find items reach it
        // down the responder chain through `performFindPanelAction(_:)`.
        textView.usesFindBar = true
        textView.isIncrementalSearchingEnabled = true

        let scrollView = NSScrollView()
        scrollView.documentView = textView
        scrollView.hasVerticalScroller = true
        scrollView.autohidesScrollers = true
        scrollView.drawsBackground = true
        scrollView.backgroundColor = Theme.background.nsColor
        scrollView.borderType = .noBorder

        coordinator.attach(textView)
        // Before the first `syncText`: the buffer is attributed with
        // `baseAttributes`, which do not exist until a size has been applied.
        coordinator.applyStyle()
        coordinator.syncText(text)
        coordinator.applySelection(selection, in: text)
        return scrollView
    }

    func updateNSView(_ scrollView: NSScrollView, context: Context) {
        let coordinator = context.coordinator
        // The coordinator outlives this struct, so the bindings it writes
        // through have to be replaced on every pass or it keeps writing into a
        // value that stopped being current several updates ago.
        coordinator.parent = self
        guard let textView = coordinator.textView else { return }

        coordinator.applyStyle()
        coordinator.syncText(text)
        coordinator.applySelection(selection, in: text)

        // Claimed only as focus arrives, never re-claimed while it stays. This
        // pane has a result grid under it that takes first responder for itself
        // when clicked, and re-asserting on every update — the grid delivering a
        // row is an update — would snatch the caret back out of it.
        if isFocused, !coordinator.wasFocused, let window = textView.window,
            window.firstResponder !== textView
        {
            window.makeFirstResponder(textView)
        }
        coordinator.wasFocused = isFocused
    }

    /// Bridges the text view to the two bindings, and owns the highlighting.
    ///
    /// One object rather than a delegate and a separate highlighter: the token
    /// cache has to be invalidated by exactly the events the delegate hears
    /// about, and splitting them would mean one of them holding a reference to
    /// the other for no other purpose.
    final class Coordinator: NSObject, NSTextViewDelegate {
        var parent: SQLEditor
        private(set) weak var textView: NSTextView?

        /// The colours for the visible text, in the UTF-16 units AppKit counts
        /// in. Recomputed when the text changes and reused while the user
        /// scrolls, which is what keeps a scroll from paying for a re-lex.
        private var painted: [(range: NSRange, kind: SQLScript.Token.Kind)] = []

        /// Set while a binding is being written into the text view, so that the
        /// delegate callbacks AppKit fires in response do not write it straight
        /// back — and, worse, do not mutate observable state in the middle of a
        /// SwiftUI update pass.
        private var applyingBinding = false

        /// The selection last pushed out to the model, to avoid a write per
        /// caret blink's worth of notification. Compared as an `NSRange`
        /// because `TextSelection` is not `Equatable`.
        private var pushedSelection: NSRange?

        /// The buffer as this object last saw it, in native storage. Held so
        /// that a caret move need not fetch and re-measure the whole text.
        private var cachedString = ""

        /// The pair of parentheses the caret is beside, in scalar offsets, or
        /// nothing where there is no pair to mark.
        private var brackets: (Int, Int)?

        /// A caret the model moved that has not been scrolled to yet. See
        /// `reveal()`.
        private var pendingReveal: NSRange?

        /// Whether a deferred repaint is already queued, so a flick of the
        /// trackpad's worth of bounds notifications collapses into one.
        private var refreshScheduled = false

        /// Whether SwiftUI's focus was already here on the previous update. See
        /// `updateNSView`.
        var wasFocused = false

        /// The list of names under the caret, when there is one.
        private let popup = CompletionPopup()

        /// Which question the popup is showing the answer to. An answer arrives
        /// after a round trip that a fast typist outruns, and one that lands
        /// against a buffer that has moved on would offer names for text that is
        /// no longer there — and, worse, name a range to replace that now covers
        /// different characters.
        private var asked = 0

        /// Set while an offer is being inserted, so the edit that inserts it is
        /// not read as the user typing something new to complete.
        private var accepting = false

        /// The attributes every character starts from. Instance state rebuilt
        /// by `applyStyle` rather than the constant it used to be, because the
        /// font in them is now sized by a preference.
        private(set) var baseAttributes: [NSAttributedString.Key: Any] = [:]

        /// The size and tab width last applied, so the SwiftUI updates that
        /// have nothing to do with either — which is most of them — cost a
        /// comparison rather than a re-attribution of the whole buffer.
        private var applied: (fontSize: Int, tabWidth: Int) = (0, 0)

        init(_ parent: SQLEditor) {
            self.parent = parent
        }

        /// Puts the preferred type size and tab width onto the view, the
        /// typing attributes and every character already in the buffer.
        ///
        /// The buffer is restyled through the storage rather than by `setText`,
        /// which would also reset the selection and the undo stack — a size
        /// change is a change to how the text looks, not to what it says.
        ///
        /// The tab width rides in the paragraph style because that is the only
        /// place AppKit reads one: a hard tab advances to the next multiple of
        /// `defaultTabInterval`, and the interval has to be restated whenever
        /// the font is, since it is that font's space that measures a column.
        func applyStyle() {
            guard let textView,
                applied != (parent.fontSize, parent.typing.tabWidth)
            else { return }
            applied = (parent.fontSize, parent.typing.tabWidth)
            let font = NSFont.monospacedSystemFont(
                ofSize: CGFloat(parent.fontSize), weight: .regular)
            let paragraph = NSMutableParagraphStyle()
            paragraph.tabStops = []
            paragraph.defaultTabInterval =
                (" " as NSString).size(withAttributes: [.font: font]).width
                * CGFloat(parent.typing.tabWidth)
            baseAttributes = [
                .font: font,
                .foregroundColor: Theme.Editor.text.nsColor,
                .paragraphStyle: paragraph
            ]
            textView.font = font
            textView.defaultParagraphStyle = paragraph
            textView.typingAttributes = baseAttributes
            popup.fontSize = CGFloat(parent.fontSize)
            if let storage = textView.textStorage, storage.length > 0 {
                storage.addAttributes(
                    baseAttributes, range: NSRange(location: 0, length: storage.length))
            }
        }

        func attach(_ textView: NSTextView) {
            self.textView = textView
            let center = NotificationCenter.default
            // Two triggers, because the visible range moves for two reasons.
            // The clip view's bounds change when the user scrolls; the text
            // view's frame changes when the pane is resized — and once more when
            // it acquires its first real size, which is the pass that paints a
            // buffer opened from `--sql`.
            if let clip = textView.enclosingScrollView?.contentView {
                clip.postsBoundsChangedNotifications = true
                center.addObserver(
                    self, selector: #selector(viewportMoved),
                    name: NSView.boundsDidChangeNotification, object: clip)
            }
            textView.postsFrameChangedNotifications = true
            center.addObserver(
                self, selector: #selector(viewportMoved),
                name: NSView.frameDidChangeNotification, object: textView)
        }

        /// Repainting is deferred a turn when the viewport is what moved.
        ///
        /// Both notifications arrive from inside AppKit's own layout and scroll
        /// work, and both handlers here reach back into the layout manager —
        /// `reveal` forces layout and scrolls, `highlight` invalidates display
        /// for a range the clip view may be part way through blitting. Doing
        /// either in place corrupts the frame: it drew a line's glyphs at the
        /// wrong origin, and once left three characters of another line's text
        /// behind, with nothing afterwards to invalidate it back. A scroll can
        /// afford the turn, because the painted margin is a screen deep either
        /// way and nothing newly on screen is waiting for this. A keystroke
        /// cannot, so `textDidChange` still paints in place — it runs after the
        /// edit rather than during it, which is a safe moment.
        @objc private func viewportMoved() {
            scheduleViewportRefresh()
        }

        private func scheduleViewportRefresh() {
            guard !refreshScheduled else { return }
            refreshScheduled = true
            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
                refreshScheduled = false
                reveal()
                highlight()
            }
        }

        // MARK: - Text

        /// Takes a buffer the model changed under the editor, if it did.
        ///
        /// Against the cached copy rather than `NSTextView.string`, which would
        /// bridge and walk the whole buffer to answer a question asked on every
        /// SwiftUI update — most of which have nothing to do with this pane.
        func syncText(_ text: String) {
            guard cachedString != text else { return }
            setText(text)
        }

        /// Replaces the buffer wholesale, for a change that came from the model
        /// rather than from typing.
        private func setText(_ text: String) {
            guard let textView, let storage = textView.textStorage else { return }
            applyingBinding = true
            storage.setAttributedString(
                NSAttributedString(string: text, attributes: baseAttributes))
            applyingBinding = false
            cachedString = text
            relex(text)
        }

        func textDidChange(_ notification: Notification) {
            guard !applyingBinding, let textView else { return }
            // `NSTextView.string` hands back a `String` lazily bridged from the
            // `NSMutableString` the storage keeps, and every scalar read through
            // that bridge takes the slow path. Measured over a 140 KB buffer:
            // lexing it costs 3.4 ms bridged and 1.9 ms native, and converting
            // the token offsets 1.0 ms against 0.2 ms. One conversion here is
            // paid back several times before the keystroke is over.
            var string = textView.string
            string.makeContiguousUTF8()
            cachedString = string
            relex(string)
            parent.text = string
            pushSelection(in: string)
            if !accepting { askForOffers(in: string) }
        }

        func textViewDidChangeSelection(_ notification: Notification) {
            // Against the cached buffer rather than a fresh copy of it. Moving
            // the caret fires this on every arrow key, and the text has not
            // changed, so fetching and comparing the whole buffer to discover
            // that would be the most expensive thing a cursor key does.
            //
            // AppKit is free to fire this before the edit that caused it. Then
            // the cache is a buffer shorter than the new caret, `Range(_:in:)`
            // rejects the conversion, and nothing is pushed — which is right,
            // because `textDidChange` is about to push both halves together.
            guard !applyingBinding else { return }
            // Any caret move that is not the one the offers were asked about
            // ends them. An arrow key, a click, an accepted offer: in every case
            // the list on screen describes a place the caret has left.
            popup.hide()
            pushSelection(in: cachedString)
            markBrackets(in: cachedString)
            highlight()
        }

        func textDidEndEditing(_ notification: Notification) {
            popup.hide()
        }

        // MARK: - Completion

        /// Intercepts the keys the list answers to while it is up, and the two
        /// the typing rules may claim when it is not.
        ///
        /// Returning true takes the key: ↑ and ↓ move the selection instead of
        /// the caret, Return and Tab accept, Escape dismisses. Every one of them
        /// means something else in a text view, which is why none of them is
        /// taken when there is no list to take it for — an editor where Return
        /// sometimes does not insert a newline is worse than one with no
        /// completion at all.
        ///
        /// With no list up, Return and Tab are offered to `EditorTyping`, whose
        /// answer is nil exactly when the plain keystroke is right — so the
        /// default path stays the default rather than being re-implemented
        /// here.
        func textView(
            _ textView: NSTextView, doCommandBy command: Selector
        ) -> Bool {
            if popup.isVisible {
                switch command {
                case #selector(NSResponder.moveUp(_:)):
                    popup.move(by: -1)
                case #selector(NSResponder.moveDown(_:)):
                    popup.move(by: 1)
                case #selector(NSResponder.insertNewline(_:)),
                    #selector(NSResponder.insertTab(_:)):
                    accept()
                case #selector(NSResponder.cancelOperation(_:)):
                    popup.hide()
                default:
                    return false
                }
                return true
            }
            switch command {
            case #selector(NSResponder.insertNewline(_:)):
                return apply(
                    EditorTyping.newline(
                        in: cachedString, selection: scalarSelection(), rules: parent.typing))
            case #selector(NSResponder.insertTab(_:)):
                return apply(
                    EditorTyping.tab(
                        in: cachedString, selection: scalarSelection(), rules: parent.typing))
            default:
                return false
            }
        }

        /// Offers a typed string to the auto-pair rule and applies its answer.
        /// False when the plain insertion is right, which hands the keystroke
        /// back to the text view.
        ///
        /// Guarded against `accepting` because an accepted completion arrives
        /// through `insertText` too, and an offer ending in `(` must not grow
        /// a second parenthesis on the way in.
        fileprivate func pair(_ typed: String) -> Bool {
            guard !accepting else { return false }
            return apply(
                EditorTyping.pairedInsertion(
                    of: typed, in: cachedString, selection: scalarSelection(),
                    rules: parent.typing))
        }

        /// The selection as the scalar offsets the typing rules read.
        private func scalarSelection() -> Range<Int> {
            guard let textView else { return 0..<0 }
            return Self.scalarRange(of: textView.selectedRange(), in: cachedString)
        }

        /// Carries one of the rules' edits into the buffer through the path
        /// typing takes, so it is undoable and reaches the model like any
        /// keystroke. False for no edit, which hands the key back to AppKit.
        ///
        /// The selection is converted against `cachedString` *after* the
        /// insertion, because `insertText` has already run `textDidChange` by
        /// the time it returns — the edit's offsets index the text it produced,
        /// not the one it replaced.
        private func apply(_ edit: EditorTyping.Edit?) -> Bool {
            guard let edit, let textView,
                let replacing = SQLCompletion.utf16Range(of: edit.replacing, in: cachedString)
            else { return false }
            if !edit.insert.isEmpty || !edit.replacing.isEmpty {
                textView.insertText(edit.insert, replacementRange: replacing)
            }
            if let target = SQLCompletion.utf16Range(of: edit.selection, in: cachedString),
                target != textView.selectedRange()
            {
                textView.setSelectedRange(target)
            }
            return true
        }

        /// Asks the core what could be typed, if what was just typed asks for
        /// it.
        ///
        /// `unprompted` is the user asking outright, which skips the rule about
        /// what was typed: pressing ⌥Esc after `FROM ` means "tell me the
        /// tables", and that is exactly the position the automatic trigger stays
        /// out of.
        /// The buffer as it stands, for the caller that has no copy of its own.
        fileprivate func askForOffers(unprompted: Bool) {
            askForOffers(in: cachedString, unprompted: unprompted)
        }

        private func askForOffers(in string: String, unprompted: Bool = false) {
            guard let textView else { return }
            let selection = Self.scalarRange(of: textView.selectedRange(), in: string)
            // Nothing is completed into a selection: accepting would replace
            // text the user deliberately highlighted, which is not what a list
            // of names is offering to do.
            guard selection.isEmpty else {
                popup.hide()
                return
            }
            let caret = selection.lowerBound
            guard unprompted || SQLCompletion.wantsOffers(before: caret, in: string) else {
                popup.hide()
                return
            }
            asked += 1
            let generation = asked
            parent.offers(string, caret) { [weak self] answer in
                guard let self, generation == asked else { return }
                present(answer, askedAbout: string, at: caret)
            }
        }

        /// Puts an answer on screen, under the caret it was asked about.
        ///
        /// The buffer and the caret are checked against the ones the question
        /// was asked about, and not merely the question against the last one
        /// sent. A caret that moved without an edit — an arrow key, a click —
        /// asks nothing new, so the counter alone would let an answer about the
        /// place the user just left open a list under the place they went to.
        private func present(_ answer: SQLCompletion.Answer, askedAbout text: String, at caret: Int)
        {
            guard let textView, let window = textView.window, !answer.offers.isEmpty,
                cachedString == text,
                Self.scalarRange(of: textView.selectedRange(), in: text) == caret..<caret
            else {
                popup.hide()
                return
            }
            // Screen coordinates, from the text view's own layout — the caret
            // may be anywhere in a scrolled buffer, and this is the one thing
            // that knows where it ended up on the display.
            var actual = NSRange()
            let caret = textView.selectedRange()
            let rect = textView.firstRect(
                forCharacterRange: NSRange(location: caret.location, length: 0),
                actualRange: &actual)
            popup.onAccept = { [weak self] in self?.accept() }
            popup.show(answer.offers, replacing: answer.replacing, under: rect, in: window)
        }

        /// Puts the selected offer into the buffer.
        ///
        /// Through `insertText(_:replacementRange:)` rather than by editing the
        /// storage, so that accepting is one undo step, the caret lands after
        /// what was inserted, and the change reaches the model down the same
        /// path typing does.
        private func accept() {
            guard let textView, let offer = popup.selectedOffer,
                let range = SQLCompletion.utf16Range(of: popup.replacing, in: cachedString),
                NSMaxRange(range) <= (textView.textStorage?.length ?? 0)
            else {
                popup.hide()
                return
            }
            popup.hide()
            accepting = true
            textView.insertText(offer.insert, replacementRange: range)
            accepting = false
        }

        /// Sends the caret back to the model.
        ///
        /// Always after the text it indexes. A `TextSelection` carries
        /// `String.Index` values, which are offsets into a string that stopped
        /// existing the moment an edit landed; pushing a caret that indexes past
        /// the end of the model's buffer is how ⌘R ends up reading a statement
        /// out of a string it was never measured against.
        private func pushSelection(in string: String) {
            guard let textView else { return }
            let range = textView.selectedRange()
            guard range != pushedSelection,
                let selection = Self.selection(for: range, in: string)
            else { return }
            pushedSelection = range
            parent.selection = selection
        }

        /// Puts the model's caret into the text view, when it is somewhere else.
        ///
        /// The equality test is not an optimisation. Called from
        /// `updateNSView`, which runs for reasons that have nothing to do with
        /// the editor — a row arriving in the grid below is one — and setting
        /// the selection unconditionally would drag the caret back to wherever
        /// the model last saw it, every time.
        func applySelection(_ selection: TextSelection?, in text: String) {
            guard let textView, let storage = textView.textStorage,
                let target = Self.range(for: selection, in: text)
            else { return }
            guard target != textView.selectedRange(), NSMaxRange(target) <= storage.length
            else { return }
            applyingBinding = true
            textView.setSelectedRange(target)
            applyingBinding = false
            pushedSelection = target
            pendingReveal = target
            // Through the same deferral as a scroll: this is reached from
            // `updateNSView`, which SwiftUI may well be running inside a layout
            // pass of its own.
            scheduleViewportRefresh()
        }

        /// Scrolls the caret the model just moved into view.
        ///
        /// Held pending rather than done on the spot when the text view has no
        /// size yet, which is the state it is in for the whole of `makeNSView`.
        /// A buffer opened from `--sql` with a caret hundreds of lines down
        /// would otherwise be scrolled to a viewport that does not exist, and
        /// then left at the top; the same window is where `pointAtSyntaxError`
        /// would lose an error that is off screen.
        private func reveal() {
            guard let textView, let target = pendingReveal, !textView.visibleRect.isEmpty
            else { return }
            // Cleared first: scrolling posts the bounds notification that calls
            // back in here.
            pendingReveal = nil
            textView.scrollRangeToVisible(target)
        }

        private static func selection(for range: NSRange, in text: String) -> TextSelection? {
            guard let indices = Range(range, in: text) else { return nil }
            return indices.isEmpty
                ? TextSelection(insertionPoint: indices.lowerBound)
                : TextSelection(range: indices)
        }

        private static func range(for selection: TextSelection?, in text: String) -> NSRange? {
            guard let indices = selection?.indices else { return nil }
            switch indices {
            case .selection(let range):
                return NSRange(range, in: text)
            case .multiSelection(let set):
                // The same reading `AppModel.editorSelection` takes: a
                // discontiguous selection names no single place, so its first
                // run is what the editor shows.
                guard let first = set.ranges.first else { return nil }
                return NSRange(first, in: text)
            @unknown default:
                return nil
            }
        }

        // MARK: - Colour

        /// Works out which pair `highlight()` should mark, from where the caret
        /// is now.
        ///
        /// The scan is memoized on its arguments, so asking for it here costs
        /// nothing on a keystroke that already lexed — which is why this reads
        /// the selection the same way `relex` does rather than being handed the
        /// answer.
        private func markBrackets(in string: String) {
            guard let textView else { return }
            let selection = Self.scalarRange(of: textView.selectedRange(), in: string)
            brackets = SQLScript.scan(
                string, scheme: parent.scheme,
                selection: selection
            ).brackets(atCaret: selection.lowerBound, in: string)
        }

        /// Asks the core to read the buffer again and repaints what is on
        /// screen.
        ///
        /// The buffer is scanned whole on every edit, and that is a decision
        /// rather than the path of least resistance. Nothing cheaper is
        /// correct: typing one `/*` at the top of a script turns every line
        /// below it into a comment, and a `'` does the same with a literal, so
        /// there is no line the scanner can safely resume from without having
        /// carried its state there from the beginning of the file. Keeping a
        /// per-line state cache to make that possible is a real design, and it
        /// buys nothing at the sizes this editor sees.
        ///
        /// The caret is handed over with the text because one scan answers the
        /// colours and the run target together. The model reads the target
        /// during the SwiftUI update this keystroke is about to trigger, and
        /// asking here with the caret it is about to be given means that read
        /// finds the answer already made — one crossing per keystroke rather
        /// than two.
        ///
        /// What is expensive at any size is handing TextKit an attribute range
        /// per token, each invalidating layout where it lands. So the tokens are
        /// cached and only the ones on screen are applied, which holds the
        /// repaint flat whatever the buffer is doing and keeps the scanner out
        /// of scrolling altogether.
        private func relex(_ string: String) {
            guard let textView else { return }
            let scan = SQLScript.scan(
                string, scheme: parent.scheme,
                selection: Self.scalarRange(of: textView.selectedRange(), in: string))
            painted = Self.utf16Ranges(scan.tokens, in: string)
            markBrackets(in: string)
            highlight()
        }

        /// An AppKit selection as the scalar offsets the core counts in.
        ///
        /// `NSRange` has always meant UTF-16 units and the core counts Unicode
        /// scalars; the two agree on every character in the Basic Multilingual
        /// Plane and disagree on every emoji. Nothing selected for a range the
        /// buffer cannot hold, which is what a selection left over from the text
        /// this one replaced looks like.
        private static func scalarRange(of range: NSRange, in text: String) -> Range<Int> {
            guard let indices = Range(range, in: text) else { return 0..<0 }
            let scalars = text.unicodeScalars
            let lower = scalars.distance(from: scalars.startIndex, to: indices.lowerBound)
            let length = scalars.distance(from: indices.lowerBound, to: indices.upperBound)
            return lower..<(lower + length)
        }

        /// Paints the visible range from the cache.
        ///
        /// Through the layout manager's *temporary* attributes, not the text
        /// storage's. Temporary attributes are display-only: they never enter
        /// the storage, so they do not land in the undo stack, are not carried
        /// into the pasteboard by a copy, and — the reason that matters here —
        /// cannot disturb the insertion point, which an edit to the storage
        /// under a live caret can.
        private func highlight() {
            guard let textView, let layout = textView.layoutManager,
                let container = textView.textContainer, let storage = textView.textStorage
            else { return }

            // The storage's own length, which is already the UTF-16 count
            // `NSRange` wants. Asking the `String` for it would mean measuring
            // the whole buffer again on every scroll.
            let length = storage.length
            guard length > 0 else { return }
            let visible = Self.visibleRange(layout, container, textView.visibleRect, length)
            guard visible.length > 0 else { return }

            layout.setTemporaryAttributes([:], forCharacterRange: visible)
            // Tokens do not overlap and arrive in order, so the first one that
            // can matter is the first whose end is past the top of the viewport
            // — which is not the same as the first that starts there. A function
            // body beginning far above the fold is one token, and finding it by
            // its start would leave it grey.
            var i = Self.firstTokenEnding(after: visible.location, in: painted)
            while i < painted.count, painted[i].range.location < NSMaxRange(visible) {
                layout.addTemporaryAttributes(
                    Self.colours[painted[i].kind] ?? [:],
                    forCharacterRange: NSIntersectionRange(painted[i].range, visible))
                i += 1
            }
            // The matched pair, if any, as a band behind each paren. A
            // background rather than a foreground colour, so the two parens
            // keep whatever colour they already have. Intersected with the
            // viewport the way the tokens are, so a partner scrolled off
            // screen costs nothing.
            if let (opening, closing) = brackets {
                for offset in [opening, closing] {
                    guard let range = SQLScript.range(offset..<(offset + 1), in: cachedString)
                    else { continue }
                    layout.addTemporaryAttributes(
                        [.backgroundColor: Theme.Editor.bracketMatch.nsColor],
                        forCharacterRange: NSIntersectionRange(
                            NSRange(range, in: cachedString), visible))
                }
            }
        }

        /// Built once. The alternative spells an `NSColor` and a dictionary into
        /// existence per token per repaint, which is a few hundred allocations
        /// every time the viewport moves a line.
        private static let colours: [SQLScript.Token.Kind: [NSAttributedString.Key: Any]] = [
            .keyword: [.foregroundColor: Theme.Editor.keyword.nsColor],
            .string: [.foregroundColor: Theme.Editor.string.nsColor],
            .quotedIdentifier: [.foregroundColor: Theme.Editor.quotedIdentifier.nsColor],
            .number: [.foregroundColor: Theme.Editor.number.nsColor],
            .comment: [.foregroundColor: Theme.Editor.comment.nsColor],
            .dollarQuoted: [.foregroundColor: Theme.Editor.dollarQuoted.nsColor]
        ]

        /// The characters on screen, widened by a screen's worth either way.
        ///
        /// The margin is what keeps a fast scroll from showing a band of grey:
        /// the bounds notification arrives before the redraw, but a trackpad
        /// fling covers ground between frames, and text that was already painted
        /// costs nothing to have painted early.
        private static func visibleRange(
            _ layout: NSLayoutManager, _ container: NSTextContainer, _ rect: CGRect, _ length: Int
        ) -> NSRange {
            guard !rect.isEmpty else { return NSRange(location: 0, length: 0) }
            let margin = rect.height
            let widened = rect.insetBy(dx: 0, dy: -margin)
            let glyphs = layout.glyphRange(forBoundingRect: widened, in: container)
            let characters = layout.characterRange(forGlyphRange: glyphs, actualGlyphRange: nil)
            return NSIntersectionRange(characters, NSRange(location: 0, length: length))
        }

        private static func firstTokenEnding(
            after location: Int, in tokens: [(range: NSRange, kind: SQLScript.Token.Kind)]
        ) -> Int {
            var low = 0
            var high = tokens.count
            while low < high {
                let mid = (low + high) / 2
                if NSMaxRange(tokens[mid].range) <= location { low = mid + 1 } else { high = mid }
            }
            return low
        }

        /// Token offsets, converted from Unicode scalars to UTF-16 units.
        ///
        /// The core counts scalars because that is what a server reports an
        /// error position in; AppKit counts UTF-16 units because that is what
        /// `NSRange` has always meant. The two agree on every character in the
        /// Basic Multilingual Plane and disagree on every emoji, so a literal
        /// holding one would otherwise shift every colour after it one place to
        /// the left. Converted in a single walk of the buffer, taking advantage
        /// of the tokens already being sorted, rather than per token — the
        /// obvious `String.Index` arithmetic is linear each time it is asked.
        private static func utf16Ranges(
            _ tokens: [SQLScript.Token], in text: String
        ) -> [(range: NSRange, kind: SQLScript.Token.Kind)] {
            var result: [(range: NSRange, kind: SQLScript.Token.Kind)] = []
            result.reserveCapacity(tokens.count)
            var scalars = text.unicodeScalars.makeIterator()
            var scalarOffset = 0
            var utf16Offset = 0

            func advance(to target: Int) {
                while scalarOffset < target, let scalar = scalars.next() {
                    utf16Offset += UTF16.width(scalar)
                    scalarOffset += 1
                }
            }

            for token in tokens {
                advance(to: token.range.lowerBound)
                let start = utf16Offset
                advance(to: token.range.upperBound)
                result.append(
                    (NSRange(location: start, length: utf16Offset - start), token.kind))
            }
            return result
        }
    }
}

/// The editor's text view, which completes names itself.
///
/// `NSTextView.complete(_:)` is the standard "finish what I am typing" command:
/// ⌥⎋ is bound to it, the Edit menu's Complete item sends it, and both arrive
/// here. Overridden rather than allowed through, because the popup AppKit would
/// otherwise open asks its delegate for the list synchronously — and this list
/// comes from a database.
final class EditorTextView: NSTextView {
    /// The coordinator, which owns the list. Weak because it owns this view's
    /// scroll view in turn, through SwiftUI.
    weak var editor: SQLEditor.Coordinator?

    override func complete(_ sender: Any?) {
        editor?.askForOffers(unprompted: true)
    }

    /// Where auto-pair intercepts a keystroke, because a plain character does
    /// not come through `doCommandBy` — this is the first override it reaches.
    ///
    /// Only typed text with no explicit range is offered to the rule: an IME
    /// composition and the coordinator's own edits both name a range, and a
    /// composition must reach AppKit whole or dead keys stop composing. The
    /// coordinator's insertions pass a concrete range too, which is what keeps
    /// applying a pair from re-entering this override.
    override func insertText(_ insertString: Any, replacementRange: NSRange) {
        if replacementRange.location == NSNotFound, !hasMarkedText(),
            let typed = insertString as? String,
            editor?.pair(typed) == true
        {
            return
        }
        super.insertText(insertString, replacementRange: replacementRange)
    }
}
