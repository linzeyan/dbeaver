import Foundation

/// The grid's clipboard renderings, kept out of the view so they can be
/// checked without a window: `--verify-clipboard` runs them against a table of
/// literals, the same way `EditingChecks` checks the staged changes.
///
/// `StagedRows` rather than `ArrowTable` for the same reason: the rules below
/// are about what a paste should contain, not about where the rows came from.
enum GridClipboard {
    /// A cell as it should land on the pasteboard: the value, not the way the
    /// grid spells it. NULL is empty rather than the word, which would paste
    /// into the next tool as a literal four-character string.
    static func value(of rows: StagedRows, row: Int, column: Int) -> String {
        rows.value(row: row, column: column) ?? ""
    }

    /// A multi-row selection copies as TSV with a header line — the one format
    /// a spreadsheet, a SQL console and a plain text editor all read unchanged.
    ///
    /// Built on the calling thread on purpose. A full 100,000-row selection
    /// takes a visible beat, but the alternative — building it in the
    /// background and filling the pasteboard when it finishes — means a paste
    /// issued in that window silently yields the previous clipboard. A slow
    /// copy is a worse experience than a fast one; a wrong copy is a bug.
    static func tabSeparated(_ rows: StagedRows, rows range: ClosedRange<Int>) -> String {
        var out = rows.columnNames.joined(separator: "\t")
        out.reserveCapacity(range.count * rows.columnNames.count * 12)
        for r in range {
            out.append("\n")
            for c in rows.columnNames.indices {
                if c > 0 { out.append("\t") }
                out.append(sanitized(value(of: rows, row: r, column: c)))
            }
        }
        return out
    }

    /// A tab or newline inside a value would add columns and rows that were
    /// never selected, so they collapse to spaces. The alternative — quoting —
    /// is CSV's answer and would stop this being pasteable as plain text.
    private static func sanitized(_ value: String) -> String {
        guard value.contains(where: { $0.isNewline || $0 == "\t" }) else { return value }
        return String(value.map { $0.isNewline || $0 == "\t" ? " " : $0 })
    }

    /// The same rows as RFC 4180 CSV: a header line of the column names, then
    /// one line per row, in column order.
    ///
    /// A field is wrapped in double quotes only when it contains a comma, a
    /// double quote, a carriage return or a line feed, and a double quote
    /// inside it is doubled. Nothing else is quoted — quoting every field is
    /// correct and unreadable.
    static func csv(_ rows: StagedRows, rows range: ClosedRange<Int>) -> String {
        var out = rows.columnNames.map(field).joined(separator: ",")
        for r in range {
            out.append("\n")
            out.append(
                rows.columnNames.indices
                    .map { field(value(of: rows, row: r, column: $0)) }
                    .joined(separator: ","))
        }
        return out
    }

    private static func field(_ value: String) -> String {
        guard value.contains(where: { $0 == "," || $0 == "\"" || $0 == "\r" || $0 == "\n" })
        else { return value }
        return "\"" + value.replacingOccurrences(of: "\"", with: "\"\"") + "\""
    }
}
