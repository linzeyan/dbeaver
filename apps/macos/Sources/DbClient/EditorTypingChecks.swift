import Foundation

/// Executable checks for the editor's typing rules, run by
/// `--verify-editor-typing`.
///
/// The rules are pure string-in, string-out — that is why they live in
/// `EditorTyping` rather than in the text view — so what is checked here is the
/// rules themselves, at the boundaries where each earns its keep. Every rule is
/// also run with its setting off and asserted nil, because nil is the contract
/// with the editor: it means "the plain keystroke is right", and a rule that
/// answered something under an off switch would be a setting wired to the wrong
/// side — the mistake that compiles, passes a default-only check, and surprises
/// the first person to open Settings.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
enum EditorTypingChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkReturnCarriesTheIndentOnlyWhileTheSettingSaysSo()
        checkReturnInsideTheIndentTakesOnlyWhatTheCaretStandsAfter()
        checkReturnReplacesTheSelectionItLandsOn()
        checkTabBecomesSpacesOnlyWhileTheSettingSaysSo()
        checkSoftTabsStopAtTheColumnsHardTabsWouldReach()
        if failures == 0 {
            fputs("editor-typing: all checks passed\n", stderr)
        } else {
            fputs("editor-typing: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Auto-indent

    /// Return reproduces the line's leading whitespace — verbatim, tabs
    /// included — and does nothing at all with the setting off.
    private static func checkReturnCarriesTheIndentOnlyWhileTheSettingSaysSo() {
        expect(
            EditorTyping.newline(in: "  where x", selection: 9..<9, rules: indenting),
            EditorTyping.Edit(replacing: 9..<9, insert: "\n  ", selection: 12..<12),
            "two spaces of indent arrive on the new line, with the caret after them")
        expect(
            EditorTyping.newline(in: "\tfoo", selection: 4..<4, rules: indenting),
            EditorTyping.Edit(replacing: 4..<4, insert: "\n\t", selection: 6..<6),
            "a tab indent stays a tab — reproducing the line is the promise, not converting it")
        expect(
            EditorTyping.newline(in: "select 1", selection: 8..<8, rules: indenting),
            EditorTyping.Edit(replacing: 8..<8, insert: "\n", selection: 9..<9),
            "an unindented line gets a plain newline")
        expect(
            EditorTyping.newline(in: "  where x", selection: 9..<9, rules: plain), nil,
            "and with the setting off the rule stands aside for AppKit's own Return")
    }

    /// The caret's own column is the most the next line can inherit.
    ///
    /// Without the clip, Return at the head of an indented line would indent
    /// the new line by whitespace the caret was standing *before* — an indent
    /// taken from a place the user never was.
    private static func checkReturnInsideTheIndentTakesOnlyWhatTheCaretStandsAfter() {
        expect(
            EditorTyping.newline(in: "    select", selection: 2..<2, rules: indenting),
            EditorTyping.Edit(replacing: 2..<2, insert: "\n  ", selection: 5..<5),
            "a caret two spaces into a four-space indent carries two")
        expect(
            EditorTyping.newline(in: "    select", selection: 0..<0, rules: indenting),
            EditorTyping.Edit(replacing: 0..<0, insert: "\n", selection: 1..<1),
            "and at the head of the line carries nothing")
    }

    /// Return over a selection replaces it, the way the plain key would.
    private static func checkReturnReplacesTheSelectionItLandsOn() {
        expect(
            EditorTyping.newline(in: "  abcd", selection: 3..<5, rules: indenting),
            EditorTyping.Edit(replacing: 3..<5, insert: "\n  ", selection: 6..<6),
            "the selection goes, the indent of its first line arrives")
    }

    // MARK: - Soft tabs

    /// Tab writes spaces to the next stop, and nothing at all with the setting
    /// off.
    private static func checkTabBecomesSpacesOnlyWhileTheSettingSaysSo() {
        expect(
            EditorTyping.tab(in: "", selection: 0..<0, rules: soft(4)),
            EditorTyping.Edit(replacing: 0..<0, insert: "    ", selection: 4..<4),
            "at the head of a line, a full stop's worth")
        expect(
            EditorTyping.tab(in: "se", selection: 2..<2, rules: soft(4)),
            EditorTyping.Edit(replacing: 2..<2, insert: "  ", selection: 4..<4),
            "two columns in, only the two that reach the stop — what makes soft tabs align")
        expect(
            EditorTyping.tab(in: "sele", selection: 4..<4, rules: soft(4)),
            EditorTyping.Edit(replacing: 4..<4, insert: "    ", selection: 8..<8),
            "on a stop, the next one")
        expect(
            EditorTyping.tab(in: "se", selection: 2..<2, rules: soft(8)),
            EditorTyping.Edit(replacing: 2..<2, insert: "      ", selection: 8..<8),
            "the width setting is what names the stops")
        expect(
            EditorTyping.tab(in: "se", selection: 2..<2, rules: plain), nil,
            "and with the setting off the rule stands aside for the tab character")
    }

    /// A hard tab already in the line counts as the columns it occupies, so a
    /// line mixing the two still lands on the stops.
    private static func checkSoftTabsStopAtTheColumnsHardTabsWouldReach() {
        expect(
            EditorTyping.tab(in: "ab\t", selection: 3..<3, rules: soft(4)),
            EditorTyping.Edit(replacing: 3..<3, insert: "    ", selection: 7..<7),
            "after 'ab' and a tab the column is 4, so the next stop is 8")
        expect(
            EditorTyping.tab(in: "x\ny", selection: 3..<3, rules: soft(4)),
            EditorTyping.Edit(replacing: 3..<3, insert: "   ", selection: 6..<6),
            "and the count starts at this line, not at the top of the buffer")
    }

    // MARK: - Harness

    /// Auto-indent alone, which is the shipped default.
    private static let indenting = EditorTyping.Rules(
        tabWidth: 4, softTabs: false, autoIndent: true)

    /// Every rule off, for the nil half of each check.
    private static let plain = EditorTyping.Rules(
        tabWidth: 4, softTabs: false, autoIndent: false)

    private static func soft(_ width: Int) -> EditorTyping.Rules {
        EditorTyping.Rules(tabWidth: width, softTabs: true, autoIndent: true)
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("editor-typing FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
