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

        let textView = NSTextView(frame: .zero, textContainer: container)
        textView.delegate = coordinator
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
        textView.font = Theme.Typography.editorFont
        textView.textColor = Theme.Editor.text.nsColor
        textView.typingAttributes = Coordinator.baseAttributes

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

        let scrollView = NSScrollView()
        scrollView.documentView = textView
        scrollView.hasVerticalScroller = true
        scrollView.autohidesScrollers = true
        scrollView.drawsBackground = true
        scrollView.backgroundColor = Theme.background.nsColor
        scrollView.borderType = .noBorder

        coordinator.attach(textView)
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

        /// A caret the model moved that has not been scrolled to yet. See
        /// `reveal()`.
        private var pendingReveal: NSRange?

        /// Whether a deferred repaint is already queued, so a flick of the
        /// trackpad's worth of bounds notifications collapses into one.
        private var refreshScheduled = false

        /// Whether SwiftUI's focus was already here on the previous update. See
        /// `updateNSView`.
        var wasFocused = false

        static let baseAttributes: [NSAttributedString.Key: Any] = [
            .font: Theme.Typography.editorFont,
            .foregroundColor: Theme.Editor.text.nsColor
        ]

        init(_ parent: SQLEditor) {
            self.parent = parent
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
                NSAttributedString(string: text, attributes: Self.baseAttributes))
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
            pushSelection(in: cachedString)
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
