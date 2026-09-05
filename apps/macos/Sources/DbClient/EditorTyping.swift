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
        var autoPairs: Bool
        var uppercasesKeywords: Bool
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

    /// What typing one character becomes when brackets and quotes pair: the
    /// partner arrives around the caret, a selection is wrapped instead of
    /// replaced, and typing a closer that is already at the caret walks past
    /// it. Nil whenever the plain insertion is right — including for every
    /// character that is not `(`, `[`, `'` or `"`.
    ///
    /// Walking past is what makes the pair cost nothing to people who type
    /// both halves out of habit: their closing keystroke lands where their
    /// hands expect the caret, instead of minting a second closer. The rule is
    /// by adjacency rather than by remembering which closer this editor
    /// inserted — Sequel Ace tracks that with a text attribute, and the
    /// machinery buys one distinction (walking past a closer somebody typed
    /// themselves) that adjacency gets wrong in no case anyone has named.
    ///
    /// A quote does not pair against a word — `don` + `'` must be `don'`, not
    /// `don''` — where a bracket does, because `f(` opening a call is exactly
    /// where the pair earns its keep. Same exception Sequel Ace makes.
    static func pairedInsertion(
        of typed: String, in text: String, selection: Range<Int>, rules: Rules
    ) -> Edit? {
        guard rules.autoPairs, typed.unicodeScalars.count == 1,
            let scalar = typed.unicodeScalars.first
        else { return nil }
        let scalars = Array(text.unicodeScalars)

        if selection.isEmpty, ")]'\"".unicodeScalars.contains(scalar),
            selection.lowerBound < scalars.count, scalars[selection.lowerBound] == scalar
        {
            let after = selection.lowerBound + 1
            return Edit(replacing: selection, insert: "", selection: after..<after)
        }

        let closer: Unicode.Scalar
        switch scalar {
        case "(": closer = ")"
        case "[": closer = "]"
        case "'", "\"": closer = scalar
        default: return nil
        }

        if !selection.isEmpty {
            guard selection.upperBound <= scalars.count else { return nil }
            let wrapped = String(
                String.UnicodeScalarView(scalars[selection.lowerBound..<selection.upperBound]))
            return Edit(
                replacing: selection,
                insert: typed + wrapped + String(closer),
                selection: (selection.lowerBound + 1)..<(selection.upperBound + 1))
        }

        if scalar == "'" || scalar == "\"" {
            let caret = selection.lowerBound
            if caret > 0, caret <= scalars.count, isWord(scalars[caret - 1]) { return nil }
            if caret < scalars.count, isWord(scalars[caret]) { return nil }
        }

        let caret = selection.lowerBound + 1
        return Edit(
            replacing: selection, insert: typed + String(closer), selection: caret..<caret)
    }

    /// The keyword the caret just finished, lifted to upper case — or nil when
    /// there is nothing to lift. Called as a separator is typed, which is what
    /// "finished" means: the caret stands at the end of a word and the next
    /// keystroke is leaving it.
    ///
    /// Whether the word *is* a keyword is the core lexer's answer, not a word
    /// list kept here — the same one-opinion rule the whole editor runs on,
    /// so the words this lifts are exactly the words the colours call
    /// keywords, dialect included. The lexer names keywords without context,
    /// so a comment or a string keeps its `select` (those are other token
    /// kinds), while an unquoted column deliberately called `order` would be
    /// lifted — the cost the setting's explanation owns up to.
    ///
    /// A word already upper case answers nil rather than an identical edit,
    /// because the editor applies edits through the undo stack and a no-op
    /// there is a keystroke ⌘Z gives back nothing for.
    static func keywordUpcase(
        in text: String, selection: Range<Int>, scheme: String, rules: Rules
    ) -> Edit? {
        guard rules.uppercasesKeywords, selection.isEmpty else { return nil }
        let scalars = Array(text.unicodeScalars)
        let caret = min(selection.lowerBound, scalars.count)
        guard caret >= scalars.count || !isWord(scalars[caret]) else { return nil }
        var start = caret
        while start > 0, isWord(scalars[start - 1]) { start -= 1 }
        guard start < caret else { return nil }
        let word = String(String.UnicodeScalarView(scalars[start..<caret]))
        let lifted = word.uppercased()
        guard lifted != word,
            SQLScript.scan(text, scheme: scheme, selection: selection).tokens
                .contains(where: { $0.kind == .keyword && $0.range == start..<caret })
        else { return nil }
        return Edit(replacing: start..<caret, insert: lifted, selection: selection)
    }

    // MARK: - Snippet placeholders

    /// The first blank at or after `offset`, or nil where the text holds none.
    ///
    /// A blank is spelt `${anything}`, and the spelling is fixed rather than a
    /// setting for the reason every other habit in this file is one line rather
    /// than an engine: a statement somebody saved last year has to still have
    /// blanks this editor recognises, and a configurable delimiter makes that
    /// depend on a preference nobody remembers changing.
    ///
    /// `${…}` and not `?` or `:name`, both of which are parameter markers some
    /// driver here really sends, nor `<…>`, which is two operators. Sequel Ace
    /// spells its snippets with the same braces (`SPTextView.insertAsSnippet`),
    /// without the numbering, the defaults and the mirrors it carries and this
    /// deliberately does not: order here is document order, which is the order
    /// the statement is read in, and an index is a second thing to keep
    /// consistent that nothing in this build reads.
    ///
    /// The word inside is never interpreted — it is a label for whoever is
    /// filling it in. What it buys is that a blank left unfilled is a syntax
    /// error in every dialect this build speaks, so the server refuses it
    /// instead of running something else.
    ///
    /// A blank does not span a line: a `}` further down the buffer is a brace
    /// somebody wrote, and reading the text between as one blank would select
    /// half a statement.
    static func placeholder(in text: String, from offset: Int) -> Range<Int>? {
        let scalars = Array(text.unicodeScalars)
        var i = max(offset, 0)
        while i + 1 < scalars.count {
            guard scalars[i] == "$", scalars[i + 1] == "{" else {
                i += 1
                continue
            }
            var end = i + 2
            while end < scalars.count, scalars[end] != "}", scalars[end] != "\n" { end += 1 }
            if end < scalars.count, scalars[end] == "}" { return i..<(end + 1) }
            i += 1
        }
        return nil
    }

    /// Where Tab goes while the selection is a blank: onto the next one, or —
    /// for the last — to a caret just after it. Nil otherwise, which hands the
    /// key back to the indent rule.
    ///
    /// The selection is the whole of this feature's state. There is no snippet
    /// session to open, nothing to release when the buffer, the tab or the
    /// window goes away, and no way for a walk begun in one editor to still be
    /// running when another is looked at. Typing over a blank ends the walk by
    /// making the selection something that is not a blank, which is exactly the
    /// moment it should end.
    ///
    /// The last blank collapses rather than standing aside, and that is the one
    /// case worth spelling out: Tab over a selection replaces it, so handing the
    /// key back there would let Tab delete the blank it had just been
    /// navigating. Deselecting says "back to ordinary editing" in the keystroke
    /// that asked for it, and the Tab after that indents as always.
    ///
    /// A caret is refused by the same comparison as everything else, without a
    /// clause of its own: a blank is at least `${}`, so an empty selection can
    /// never equal one.
    static func placeholderJump(in text: String, selection: Range<Int>) -> Edit? {
        guard placeholder(in: text, from: selection.lowerBound) == selection else { return nil }
        let landing =
            placeholder(in: text, from: selection.upperBound)
            ?? selection.upperBound..<selection.upperBound
        // An empty insertion over an empty range: nothing is written and only
        // the selection moves, which is the shape `apply` already handles.
        return Edit(
            replacing: selection.upperBound..<selection.upperBound, insert: "",
            selection: landing)
    }

    private static func isWord(_ scalar: Unicode.Scalar) -> Bool {
        scalar == "_" || CharacterSet.alphanumerics.contains(scalar)
    }

    private static func startOfLine(before offset: Int, in scalars: [Unicode.Scalar]) -> Int {
        var start = offset
        while start > 0, scalars[start - 1] != "\n" { start -= 1 }
        return start
    }
}
