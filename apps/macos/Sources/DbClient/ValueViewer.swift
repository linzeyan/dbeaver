import SwiftUI

/// Reading one cell's value in full.
///
/// The grid clips a cell to its column width and the inspector strip clips it to
/// one line, which between them answer "what is in this cell" for a number and
/// for nothing else: a 40-character `text`, a `jsonb` document that arrives on
/// one line, a `bytea` that has no text form at all. This is where the whole
/// value goes.
///
/// It lives under the strip rather than in a sheet or a popover, and that is the
/// load-bearing decision. What a reader does with a value is copy it or compare
/// it with the next row's, and the second one needs ↓ to keep working while the
/// value is on screen. A sheet takes the key window, a popover closes on the
/// first key that is not its own; either way the viewer has to be dismissed
/// before the grid can be moved, and a viewer that costs a dismissal per row is
/// one nobody opens twice. Sitting in the pane, it takes no focus at all — the
/// grid keeps first responder, ↓ moves the cursor, and this redraws with the new
/// cell. Beside the grid rather than under it would have cost width, which a
/// twenty-column result has none of to spare.

/// How a value should be shown once there is room to show it.
///
/// Decided from the column's type rather than from the look of the string.
/// Sniffing would pretty-print a `text` column that happens to hold `{}` and,
/// worse, would eventually meet a value that looks like JSON and is not.
enum ValueRendering {
    case text
    /// A `json`/`jsonb` column, by PostgreSQL's declared type. The Arrow schema
    /// cannot say it — the driver maps both to Utf8 — so this is only known for
    /// a browsed relation. The Query pane gets `.text` for the same reason the
    /// grid header falls back there: a statement's columns need not come from
    /// any relation, and matching them by name against the browsed one would
    /// claim a type they do not have.
    case json
    /// An Arrow binary column, with the cell's bytes. Carried here rather than
    /// re-read on demand because the read has to happen while the batch is
    /// alive, and the view is drawn after the model has finished with it.
    case binary([UInt8])

    /// Whether a declared PostgreSQL type is one whose values are JSON.
    static func isJSONType(_ declared: String) -> Bool {
        let name = declared.lowercased()
        return name == "json" || name == "jsonb"
    }

    /// The one-line form of a binary cell, for the strip.
    ///
    /// PostgreSQL's own `\x…` spelling rather than a spaced-pair dump: it is
    /// what psql prints and what the server accepts back, so a reader who
    /// recognises anything recognises this. Bounded because the strip is one
    /// line — the copy button hands over the whole literal.
    static func preview(bytes: [UInt8]) -> String {
        let shown = bytes.prefix(64)
        return "\\x" + hex(shown) + (bytes.count > shown.count ? "…" : "")
    }

    static func hex(_ bytes: some Sequence<UInt8>) -> String {
        bytes.map { String(format: "%02x", $0) }.joined()
    }
}

/// A value as the viewer will draw it, plus one line saying what was done to it.
///
/// The sentence is not decoration. Pretty-printed JSON and a hex dump are both
/// this program's rendering of the value rather than the value, and a clipped
/// one is only part of it; saying so is the same obligation the status bar meets
/// when it writes "first 100,000 of ~1,000,000 rows".
struct RenderedValue {
    let text: String
    /// What the strip reads while the viewer is open, in place of the one-line
    /// preview the pane below has made redundant.
    let descriptor: String
    /// Whether `text` stands in for a value with nothing to show — NULL, or an
    /// empty string. Drawn dimmed, because it is this program talking rather
    /// than data.
    let isPlaceholder: Bool
    /// Whether lines may be re-flowed to the pane's width. Prose wants it;
    /// indented JSON and a hex dump are laid out in columns that a soft wrap
    /// destroys, and a wrapped line there reads as part of the next one.
    let wraps: Bool

    /// How much of a value the pane will lay out.
    ///
    /// A `text` column can hold a megabyte, and handing that to a `Text` costs a
    /// full TextKit layout on every arrow key. The cap is generous enough that
    /// no ordinary value meets it and small enough that the pathological one
    /// does not lock the window.
    ///
    /// Not private, because `ValueEdit` bounds the editor at the same number.
    /// The two are one decision: an editable box is a `TextView` doing the same
    /// layout on every *keystroke* rather than every arrow key, so a value this
    /// pane will not draw is not a value the editor can hold either. Two
    /// constants would be two chances to raise one and forget the other.
    static let characterCap = 128 * 1024

    /// How much of a binary value gets dumped.
    ///
    /// Sixteen bytes to a line, so this is 256 lines — a long scroll and about
    /// as much as anyone reads of a blob. What is worth reading is at the front
    /// anyway: magic numbers, headers, the first record. Dumping a 20 MB image
    /// in full would spend forty megabytes of string on lines nobody scrolls to.
    private static let byteCap = 4 * 1024

    /// The largest document that gets re-indented.
    ///
    /// Far above `characterCap`, and deliberately: the walk is linear and cheap
    /// beside the layout that follows it, so bounding it at what the pane draws
    /// would hand back the raw line — the one unreadable thing the viewer was
    /// opened to escape — for a document only slightly too long to fit. What
    /// this bound protects against is the value that should never have been in a
    /// column, where re-indenting would allocate a second copy of it on every
    /// arrow key.
    private static let reindentCap = 4 * 1024 * 1024

    /// Main-actor because the number formatting it borrows from `AppModel` is,
    /// and because the only caller is a view body.
    @MainActor
    static func make(from cell: AppModel.InspectedCell) -> RenderedValue {
        // NULL and "" are different values, and the pane is the one place with
        // room to say which. Both are otherwise blank, and a blank pane reads as
        // a viewer that failed to load.
        if cell.isNull {
            return RenderedValue(
                text: "NULL", descriptor: "SQL NULL — not an empty string",
                isPlaceholder: true, wraps: false)
        }
        if cell.value.isEmpty {
            return RenderedValue(
                text: "(empty)", descriptor: "zero-length text — not NULL",
                isPlaceholder: true, wraps: false)
        }

        switch cell.rendering {
        case .binary(let bytes):
            let shown = bytes.prefix(byteCap)
            let clipped = bytes.count > shown.count
            return RenderedValue(
                text: hexDump(shown),
                descriptor: clipped
                    ? "hex dump of the first \(AppModel.formatted(shown.count)) "
                        + "of \(AppModel.pluralized(bytes.count, "byte"))"
                    : "hex dump · \(AppModel.pluralized(bytes.count, "byte"))",
                isPlaceholder: false, wraps: false)

        case .json:
            // Every branch measures the stored value rather than the rendering
            // of it: the number a reader checks against a `length()` is the
            // column's, not this program's.
            let measure = AppModel.pluralized(cell.value.count, "character")
            guard cell.value.count <= reindentCap else {
                return clipped(
                    cell.value, note: "too large to re-indent · \(measure)", wraps: false)
            }
            guard let pretty = prettyPrintedJSON(cell.value) else {
                // Showing nothing because the document did not parse would hide
                // the one thing the viewer was opened for.
                return clipped(
                    cell.value, note: "not valid JSON — shown as stored · \(measure)", wraps: false)
            }
            return clipped(pretty, note: "pretty-printed · \(measure)", wraps: false)

        case .text:
            return clipped(cell.value, note: nil, wraps: true)
        }
    }

    /// A rendering bounded to what the pane will lay out, saying so when it had
    /// to cut.
    ///
    /// `note` names the transformation and carries its own measure of the stored
    /// value; a rendering with none is the value itself, so its own length is
    /// the measure.
    @MainActor
    private static func clipped(_ text: String, note: String?, wraps: Bool) -> RenderedValue {
        let cut = text.count > characterCap
        let shown = AppModel.formatted(characterCap)
        let descriptor: String
        switch (cut, note) {
        case (false, let note?): descriptor = note
        case (false, nil): descriptor = AppModel.pluralized(text.count, "character")
        case (true, let note?): descriptor = "\(note) — first \(shown) shown"
        case (true, nil):
            descriptor = "first \(shown) of \(AppModel.formatted(text.count)) characters"
        }
        return RenderedValue(
            text: cut ? String(text.prefix(characterCap)) : text,
            descriptor: descriptor, isPlaceholder: false, wraps: wraps)
    }
}

/// Whether the value on screen can be edited in full, and what to say when it
/// cannot.
///
/// Answered here rather than in the view because the wrong answer is silent.
/// What the pane draws is a rendering — `RenderedValue` re-indents JSON, dumps
/// binary as hex, and cuts anything past the cap — and a box seeded from that
/// text would send this program's formatting to the server the moment it was
/// staged: a `jsonb` document rewritten with two-space indents, a `bytea`
/// replaced by the words of its own hex dump, a long `text` column truncated to
/// its first 128 K. Every one of those is an edit the user did not make and
/// would not see. So the editable case carries the *stored* string, and the
/// cases that cannot be edited honestly are refused with a sentence.
///
/// NULL and a zero-length string both seed an empty box. That is not a
/// conflation: what is typed is what gets written in either case, and the NULL
/// button on the strip is how a value goes back to being NULL. Seeding the word
/// "NULL" would put four characters in a text column, which is the mistake
/// `CellEditorRow.seed` already avoids for the one-line field.
enum ValueEdit: Equatable {
    /// The text the box starts with — the stored value, not the rendering.
    case editable(String)
    /// Why this value cannot be edited here, as a sentence to show in place of
    /// the box. Said rather than hidden: an editor that is simply absent reads
    /// as a feature this build does not have.
    case refused(String)

    /// Whether there is a box to open, for the control that opens it.
    var isEditable: Bool {
        if case .editable = self { return true }
        return false
    }

    /// The refusal, for the tooltip on the control this answer disabled. Nil
    /// where there is nothing to explain.
    var refusal: String? {
        if case .refused(let why) = self { return why }
        return nil
    }

    /// Main-actor for the reason `RenderedValue.make` is: the sentence borrows
    /// `AppModel`'s number formatting.
    @MainActor
    static func offered(for cell: AppModel.InspectedCell, obstacle: String?) -> ValueEdit {
        // The row's problem first, and before looking at the value at all. A
        // relation with no key cannot have any of its cells written, so
        // answering "this one happens to be binary" would send the reader off
        // to convert a column that was never the reason.
        if let obstacle { return .refused(obstacle) }

        if case .binary = cell.rendering {
            // The plan's judgement, and it is fail-loud rather than a
            // limitation quietly hidden: `cell.value` for a binary column is
            // `ValueRendering.preview` — the first 64 bytes and an ellipsis —
            // so a box seeded with it would offer to replace a blob with a
            // truncated transcription of itself. A hex editor is a real piece
            // of work and has not been earned; until it is, say so.
            return .refused("A binary value cannot be edited here.")
        }

        guard cell.value.count <= RenderedValue.characterCap else {
            // The length is the stored one, which is what a reader would check
            // against `length()`, and it is in the sentence because "too long"
            // without a number is not something anyone can act on.
            return .refused(
                "This value is too long to edit here — "
                    + "\(AppModel.pluralized(cell.value.count, "character")).")
        }

        return .editable(cell.isNull ? "" : cell.value)
    }
}

/// Re-indents a JSON document without changing anything in it.
///
/// Parsing into Foundation objects and re-serialising is six lines, and gets the
/// document wrong in ways that would not be noticed until they mattered:
/// `JSONSerialization` drops object key order, and rewrites every number through
/// `Double`, so `1.0` comes back `1` and an integer past 2^53 comes back
/// rounded. A viewer that shows a different document from the one stored is
/// worse than one that shows a long line.
///
/// So this moves whitespace and nothing else. Every literal — string, number,
/// keyword — is copied through character for character; the walk exists only to
/// find the structural boundaries between them.
///
/// Returns nil for input it cannot account for: an unterminated string, a
/// bracket that closes the wrong thing, a document that ends mid-structure. A
/// confidently mis-indented value is worse than the raw line, which is what the
/// caller falls back to.
func prettyPrintedJSON(_ text: String) -> String? {
    let chars = Array(text)
    var out = ""
    out.reserveCapacity(chars.count + chars.count / 4)
    /// Closing brackets owed, innermost last. Doubles as the indent depth.
    var owed: [Character] = []
    var i = 0

    func breakLine() {
        out.append("\n")
        out.append(String(repeating: "  ", count: owed.count))
    }

    while i < chars.count {
        let c = chars[i]
        switch c {
        case "\"":
            // Copied whole, escapes included: a `\"` inside a string is not the
            // end of it, and a `{` inside one is not structure.
            out.append(c)
            i += 1
            var closed = false
            while i < chars.count {
                let s = chars[i]
                out.append(s)
                i += 1
                if s == "\\" {
                    guard i < chars.count else { return nil }
                    out.append(chars[i])
                    i += 1
                } else if s == "\"" {
                    closed = true
                    break
                }
            }
            guard closed else { return nil }

        case "{", "[":
            let closer: Character = c == "{" ? "}" : "]"
            out.append(c)
            i += 1
            // An empty container stays on its line. `{\n}` spends three lines
            // saying that there is nothing there.
            var j = i
            while j < chars.count, chars[j].isWhitespace { j += 1 }
            if j < chars.count, chars[j] == closer {
                out.append(closer)
                i = j + 1
            } else {
                owed.append(closer)
                breakLine()
            }

        case "}", "]":
            guard owed.last == c else { return nil }
            owed.removeLast()
            breakLine()
            out.append(c)
            i += 1

        case ",":
            out.append(c)
            breakLine()
            i += 1

        case ":":
            out.append(": ")
            i += 1

        default:
            // Whitespace between tokens is this function's to place; anything
            // else is content.
            if !c.isWhitespace { out.append(c) }
            i += 1
        }
    }

    guard owed.isEmpty, !out.isEmpty else { return nil }
    return out
}

/// A binary value in the layout `hexdump -C` established: offset, sixteen bytes
/// split into two groups of eight, then the printable characters.
///
/// The gutter is what makes the dump answer questions. Half of what lands in a
/// `bytea` has text in it somewhere — a header, a key, an embedded name — and
/// finding it in the hex alone means decoding by eye.
func hexDump(_ bytes: some Collection<UInt8>) -> String {
    var out = ""
    out.reserveCapacity(bytes.count * 4 + 32)
    var offset = 0
    var line: [UInt8] = []
    line.reserveCapacity(16)

    func flush() {
        out.append(String(format: "%08x  ", offset))
        for i in 0..<16 {
            out.append(i < line.count ? String(format: "%02x ", line[i]) : "   ")
            if i == 7 { out.append(" ") }
        }
        out.append(" |")
        for b in line {
            // Printable ASCII only. A byte rendered through Latin-1 or UTF-8
            // would put a glyph of a different width in a column-aligned dump,
            // and claim a text encoding the column never declared.
            out.append(b >= 0x20 && b < 0x7F ? Character(UnicodeScalar(b)) : ".")
        }
        out.append("|")
        offset += line.count
        line.removeAll(keepingCapacity: true)
    }

    for b in bytes {
        line.append(b)
        if line.count == 16 {
            if offset > 0 { out.append("\n") }
            flush()
        }
    }
    if !line.isEmpty {
        if offset > 0 { out.append("\n") }
        flush()
    }
    return out
}

/// The pane under the inspector strip: one value, in full.
struct CellValueViewer: View {
    /// How tall this pane is, in both of the forms it takes.
    ///
    /// Shared with `CellValueEditor`, because the two have to be equal and were
    /// not: a box that is a footer taller than the value it replaced moves the
    /// grid every time the pencil is pressed, and takes the bottom row off it.
    /// Two literals were two chances to change one and not the other, and the
    /// first screenshot of the box caught exactly that.
    static let height: CGFloat = 220

    let rendered: RenderedValue

    var body: some View {
        ScrollView(rendered.wraps ? .vertical : [.vertical, .horizontal]) {
            content
                .padding(.horizontal, Theme.Space.md)
                .padding(.vertical, Theme.Space.sm)
        }
        // A two-axis scroll view centres content smaller than its viewport, so a
        // short value would land in the middle of the pane like a caption.
        .defaultScrollAnchor(.topLeading)
        // Fixed rather than proportional, and not draggable. The grid above is
        // what shrinks, and it has to shrink by a predictable amount: a viewer
        // that opens to a different height depending on the window makes the
        // rows jump by a different distance every time. Twelve monospaced lines
        // is enough to see the shape of a document and short enough to leave the
        // grid usable at the window's minimum height.
        .frame(height: Self.height)
        .background(Theme.background.color)
        .accessibilityLabel("Cell value")
    }

    @ViewBuilder
    private var content: some View {
        let text = Text(rendered.text)
            .font(Theme.Typography.mono)
            .foregroundStyle(
                rendered.isPlaceholder ? Theme.textTertiary.color : Theme.text.color
            )
            // Selectable because the other half of what a reader wants is a
            // fragment of the value, not all of it.
            .textSelection(.enabled)

        if rendered.wraps {
            text.frame(maxWidth: .infinity, alignment: .leading)
        } else {
            // `fixedSize` is what stops the layout wrapping the line to the pane
            // width; the horizontal scroll view is what makes the rest reachable.
            text.fixedSize()
        }
    }
}

/// The same pane, as a box.
///
/// Focused, which the reading pane deliberately is not. The header of this file
/// argues that the viewer must take no focus, so that ↓ keeps moving the grid
/// while a value is on screen; an editor is the one case where the opposite is
/// required. While a value is being typed the grid has to hold still, and the
/// arrow keys belong to the caret — a box you can arrow *out of* would swap the
/// cell under what you were writing.
///
/// A `TextEditor` rather than the window's `CompactField`, for the reason this
/// whole slice exists: a one-line field cannot hold a value with a line break in
/// it, and a `text` column that has one could not be honestly edited at all
/// before now.
struct CellValueEditor: View {
    let model: AppModel
    /// What the box starts with — `ValueEdit.editable`'s payload, which is the
    /// stored value and never the rendering that was on screen a moment ago.
    let seed: String
    @State private var typed = ""
    @FocusState private var focused: Bool

    /// How much of the pane the footer takes, leaving the rest for the box.
    ///
    /// Subtracted rather than added, so that the two together come to exactly
    /// `CellValueViewer.height` and the grid does not move when the pencil is
    /// pressed. See there.
    private static let footerHeight: CGFloat = 30

    /// The box, drawn the way `CompactField` draws the one-line field.
    ///
    /// A border, a fill and a focus ring, because without them this was
    /// indistinguishable from the read-only pane it replaces — the complaint
    /// `CellEditorRow` records word for word about the field it was given
    /// instead of a proper one, reproduced here at twelve times the size. This
    /// is a control that writes to a database, and the reader has to be able to
    /// see that they are inside it.
    private var box: some View {
        TextEditor(text: $typed)
            .font(Theme.Typography.mono)
            .foregroundStyle(Theme.text.color)
            // `TextEditor` draws its own opaque background, which on this theme
            // would be the one light rectangle in the window.
            .scrollContentBackground(.hidden)
            .focused($focused)
            .padding(.horizontal, Theme.Space.xs)
            .padding(.vertical, Theme.Space.xs)
            .background(
                RoundedRectangle(cornerRadius: Theme.Radius.control)
                    .fill(Theme.background.opacity(0.6).color)
            )
            .overlay(
                RoundedRectangle(cornerRadius: Theme.Radius.control)
                    .strokeBorder(
                        focused ? Theme.accent.color : Theme.separator.color,
                        lineWidth: 1)
            )
            .padding(.horizontal, Theme.Space.md)
            .padding(.vertical, Theme.Space.sm)
            // Applied after the padding, so the box and its inset together are
            // the height, rather than the height plus the inset.
            .frame(height: CellValueViewer.height - Self.footerHeight)
            .background(Theme.background.color)
            .accessibilityLabel("Edit cell value")
    }

    var body: some View {
        VStack(spacing: 0) {
            box
            footer
        }
        // Seeded and focused together, because a box that opens empty is
        // indistinguishable from a value that is empty.
        .task {
            typed = seed
            focused = true
        }
        // Escape leaves without staging. A control that can only be got out of
        // with the mouse is the complaint this pane exists to avoid.
        .onKeyPress(.escape) {
            model.isEditingValue = false
            return .handled
        }
    }

    private var footer: some View {
        HStack(spacing: Theme.Space.sm) {
            // The length as typed, which is the number somebody checking a value
            // against a column's limit is looking for.
            Text(AppModel.pluralized(typed.count, "character"))
                .font(Theme.Typography.micro)
                .foregroundStyle(Theme.textTertiary.color)
            Spacer()
            Button("Cancel") { model.isEditingValue = false }
                .help("Leave the value as it was (⎋)")
            Button("Stage") {
                model.stageEditedValue(typed)
                // Closed, because this is where the change becomes visible: the
                // cell is marked pending in the grid and the count beside Save
                // appears, and neither is on screen from inside the box.
                model.isEditingValue = false
            }
            // Return puts a line break in the box, which is the whole point of
            // the box, so the key that commits has to be a different one.
            .keyboardShortcut(.return, modifiers: .command)
            .help("Hold this value for the cell (⌘↩); nothing is sent until Save")
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: Self.footerHeight)
        .background(Theme.surfaceRaised.color)
    }
}
