import Foundation

/// Executable checks for the grid's clipboard renderings, run by
/// `--verify-clipboard`.
///
/// What lands on the pasteboard is this side's own rule, and it is the half
/// nothing else can see: a NULL that pastes as the word NULL, or a value whose
/// tab quietly adds a column, is a copy that succeeds and is wrong.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
enum GridClipboardChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkNullAndEmptyCopyAsNothing()
        checkTabSeparatedKeepsTheShapeOfTheSelection()
        checkCSVQuotesOnlyWhatNeedsIt()
        checkOneRowIsAHeaderAndOneLine()
        if failures == 0 {
            fputs("clipboard: all checks passed\n", stderr)
        } else {
            fputs("clipboard: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// A NULL and an empty string both copy as nothing.
    ///
    /// The two are different in the database — a text column can hold both, and
    /// they name different rows — and the same on a pasteboard. That is a
    /// deliberate loss: the alternative, spelling NULL as a word, would paste
    /// four literal characters into the next tool.
    private static func checkNullAndEmptyCopyAsNothing() {
        expect(GridClipboard.value(of: table, row: 1, column: 1), "", "a NULL copies as nothing")
        expect(
            GridClipboard.value(of: table, row: 2, column: 1), "",
            "an empty string copies as nothing")
    }

    /// The tab-separated form keeps the shape of the selection: a header line,
    /// one line per row, and a value's tab and newline collapsed to spaces so
    /// the paste has as many columns and rows as the selection did.
    private static func checkTabSeparatedKeepsTheShapeOfTheSelection() {
        let lines = GridClipboard.tabSeparated(table, rows: 0...3).components(separatedBy: "\n")
        expect(lines.count, 5, "a header line and one line per row")
        expect(lines[0], "id\tlabel\tnote", "the header is the column names")
        expect(
            lines[4], "4\thas newline\thas tab",
            "a newline and a tab inside a value collapse to spaces")
    }

    /// CSV quotes exactly the fields that need it — the comma one, the quote
    /// one with its quote doubled, the newline one — and leaves the plain ones
    /// bare, including the tab one, which RFC 4180 does not ask to be quoted.
    private static func checkCSVQuotesOnlyWhatNeedsIt() {
        let want =
            "id,label,note\n"
            + "1,one,plain\n"
            + "2,,\"has, comma\"\n"
            + "3,,\"has \"\" quote\"\n"
            + "4,\"has\nnewline\",has\ttab"
        expect(GridClipboard.csv(table, rows: 0...3), want, "CSV quotes only what needs it")
    }

    /// A one-row range renders a header and exactly one line — the case the
    /// menu's "Copy Row" item produces.
    private static func checkOneRowIsAHeaderAndOneLine() {
        expect(
            GridClipboard.tabSeparated(table, rows: 2...2),
            "id\tlabel\tnote\n3\t\thas \" quote",
            "one row of TSV is a header and one line")
        expect(
            GridClipboard.csv(table, rows: 1...1),
            "id,label,note\n2,,\"has, comma\"",
            "and so is one row of CSV")
    }

    // MARK: - Harness

    /// What a database can actually hold: a NULL, an empty string, and values
    /// carrying a comma, a double quote, a newline and a tab — the six shapes a
    /// rendering has to survive.
    private static let table = Rows(
        columnNames: ["id", "label", "note"],
        cells: [
            ["1", "one", "plain"],
            ["2", nil, "has, comma"],
            ["3", "", "has \" quote"],
            ["4", "has\nnewline", "has\ttab"]
        ])

    private struct Rows: StagedRows {
        let columnNames: [String]
        let cells: [[String?]]

        func value(row: Int, column: Int) -> String? { cells[row][column] }
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("clipboard FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
