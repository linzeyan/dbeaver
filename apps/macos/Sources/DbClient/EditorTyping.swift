import Foundation

/// What a keystroke becomes in the SQL editor, decided over plain strings.
///
/// The decisions live here rather than in the text view because a text view
/// cannot be run headless: `--verify-editor-typing` holds these rules still as
/// string-in, string-out cases, and the view's only job is to carry an `Edit`
/// into the buffer through the same path typing takes. The rules are
/// deliberately not an engine — no grammar, no per-language table — because
/// every one of them is a one-line habit borrowed from Sequel Ace, and the
/// simplest statement of a habit is the one a reader can check against their
/// fingers.
///
/// Offsets are Unicode scalars throughout, the unit everything around the
/// editor counts in; see `SQLScript`'s opening comment for why not
/// `Character`s.
enum EditorTyping {
    /// The switches the rules read, in one value so the editor view is handed
    /// its settings the way it is handed its scheme: as facts, not as the
    /// `Preferences` object they came from — which would also hide from
    /// SwiftUI which properties the pane depends on.
    struct Rules: Equatable {
        var tabWidth: Int
        var softTabs: Bool
        var autoIndent: Bool
    }

    /// One replacement to make instead of the keystroke's default: `insert`
    /// replaces the scalars in `replacing`, and the selection lands on
    /// `selection` — a caret when empty. Offsets in `selection` index the text
    /// as it is *after* the replacement.
    struct Edit: Equatable {
        let replacing: Range<Int>
        let insert: String
        let selection: Range<Int>
    }

    /// What Return inserts: a newline carrying the current line's leading
    /// whitespace, or nil when auto-indent is off and the plain newline will
    /// do.
    ///
    /// The indent is clipped at the caret, not taken whole: pressing Return
    /// inside the leading whitespace means the caret's own column is the most
    /// the next line can inherit, or Return at the start of an indented line
    /// would indent a line the caret never stood in. The whitespace is copied
    /// verbatim — tabs stay tabs — because reproducing the line above is the
    /// whole promise, not converting it.
    static func newline(in text: String, selection: Range<Int>, rules: Rules) -> Edit? {
        guard rules.autoIndent else { return nil }
        let scalars = Array(text.unicodeScalars)
        let caret = min(selection.lowerBound, scalars.count)
        let lineStart = startOfLine(before: caret, in: scalars)
        var indentEnd = lineStart
        while indentEnd < caret, scalars[indentEnd] == " " || scalars[indentEnd] == "\t" {
            indentEnd += 1
        }
        let insert = "\n" + String(String.UnicodeScalarView(scalars[lineStart..<indentEnd]))
        let after = selection.lowerBound + insert.unicodeScalars.count
        return Edit(replacing: selection, insert: insert, selection: after..<after)
    }

    /// What Tab inserts: spaces up to the next tab stop, or nil when soft tabs
    /// are off and the tab character will do.
    ///
    /// To the next stop rather than a fixed count, because that is what makes
    /// soft tabs indistinguishable from hard ones at the same width: Tab after
    /// two characters at width 4 writes two spaces, not four. The column is
    /// counted from the start of the line with a hard tab worth the columns it
    /// occupies, so a line mixing the two still lands on the stops.
    static func tab(in text: String, selection: Range<Int>, rules: Rules) -> Edit? {
        guard rules.softTabs else { return nil }
        let scalars = Array(text.unicodeScalars)
        let caret = min(selection.lowerBound, scalars.count)
        var column = 0
        for i in startOfLine(before: caret, in: scalars)..<caret {
            if scalars[i] == "\t" {
                column += rules.tabWidth - column % rules.tabWidth
            } else {
                column += 1
            }
        }
        let insert = String(
            repeating: " ", count: rules.tabWidth - column % rules.tabWidth)
        let after = selection.lowerBound + insert.unicodeScalars.count
        return Edit(replacing: selection, insert: insert, selection: after..<after)
    }

    private static func startOfLine(before offset: Int, in scalars: [Unicode.Scalar]) -> Int {
        var start = offset
        while start > 0, scalars[start - 1] != "\n" { start -= 1 }
        return start
    }
}
