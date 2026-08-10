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
    private static let characterCap = 128 * 1024

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
        .frame(height: 220)
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
