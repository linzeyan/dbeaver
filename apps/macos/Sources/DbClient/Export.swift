import Foundation
import UniformTypeIdentifiers

/// A delimited text format a result can be written to.
///
/// Two of them because they are read by different things: a spreadsheet opens
/// .csv without being asked, and whatever already consumes the ⌘C output of the
/// grid expects tabs.
enum DelimitedFormat: String, CaseIterable {
    case csv
    case tsv

    /// A scalar rather than a `Character`, because that is what a delimiter can
    /// actually be — and because `Character` is the type that makes the quoting
    /// rules below subtly wrong. See `DelimitedWriter.field`.
    var delimiter: Unicode.Scalar {
        switch self {
        case .csv: return ","
        case .tsv: return "\t"
        }
    }

    var fileExtension: String { rawValue }
    var label: String { rawValue.uppercased() }

    var contentType: UTType {
        switch self {
        case .csv: return .commaSeparatedText
        case .tsv: return .tabSeparatedText
        }
    }

    init?(pathExtension: String) {
        guard let match = Self(rawValue: pathExtension.lowercased()) else { return nil }
        self = match
    }
}

/// Turns a result into delimited text.
///
/// Formatting is kept apart from writing on purpose: `field` and `row` are
/// functions over strings and nothing else, so the quoting rules can be checked
/// without a window, a connection, or a file.
enum DelimitedWriter {
    /// One field, where nil is SQL NULL.
    ///
    /// RFC 4180 exactly for the quoting: a value carrying the delimiter, a
    /// double quote, CR or LF is wrapped in quotes with its own quotes doubled.
    /// Approximately-correct quoting is how one address column silently shifts
    /// every field after it one place to the left, in a file nobody re-reads.
    ///
    /// NULL and the empty string are written differently — nothing at all
    /// versus a quoted empty field — because they are different values and the
    /// difference is one people write WHERE clauses against. It is also what
    /// PostgreSQL's own COPY … FORMAT csv emits, so readers on the other side
    /// already know how to take it. The pasteboard path in `GridView` collapses
    /// NULL to empty instead, and is right to: a paste target is a text box,
    /// not a parser, and has nothing to tell the two apart with.
    ///
    /// The scan runs over Unicode scalars, not `Character`s. A Swift
    /// `Character` is a grapheme cluster, and CRLF is one cluster that compares
    /// equal to neither "\r" nor "\n" — so a value whose line break is CRLF
    /// walks straight past a `Character`-wise test, lands unquoted, and splits
    /// the record in two in whatever reads the file. Nothing about the result
    /// looks wrong until someone counts the rows.
    static func field(_ value: String?, delimiter: Unicode.Scalar) -> String {
        guard let value else { return "" }
        guard !value.isEmpty else { return "\"\"" }
        guard
            value.unicodeScalars.contains(where: {
                $0 == delimiter || $0 == "\"" || $0 == "\r" || $0 == "\n"
            })
        else { return value }

        var quoted = "\""
        quoted.reserveCapacity(value.utf8.count + 4)
        for scalar in value.unicodeScalars {
            if scalar == "\"" { quoted.unicodeScalars.append(scalar) }
            quoted.unicodeScalars.append(scalar)
        }
        quoted.unicodeScalars.append("\"")
        return quoted
    }

    /// One record, terminated.
    ///
    /// LF, not the CRLF the RFC nominates. Every parser worth writing a file
    /// for accepts LF, this is a file written on a Unix machine for Unix tools,
    /// and a stray CR is the kind of thing that costs somebody an afternoon.
    /// Quoting is where the RFC has to be followed to the letter; the line
    /// ending is where it does not.
    static func row(_ fields: [String?], delimiter: Unicode.Scalar) -> String {
        var line = ""
        for (index, value) in fields.enumerated() {
            if index > 0 { line.unicodeScalars.append(delimiter) }
            line.append(field(value, delimiter: delimiter))
        }
        line.unicodeScalars.append("\n")
        return line
    }

    /// Bytes buffered before a write. Large enough that a million rows cost a
    /// few hundred syscalls, small enough that the peak allocation is nothing.
    private static let flushBytes = 256 * 1024

    /// Writes `rows` to `url`, header first.
    ///
    /// Streams rather than building one String: a 100,000-row result is tens of
    /// megabytes of text, and holding all of it — plus the `Data` copy of it —
    /// before a single byte reaches the disk is a spike taken for nothing.
    ///
    /// Every failure throws, including the ones that only appear part way
    /// through. A save that quietly wrote half a file is worse than one that
    /// says the disk is full.
    static func write(
        _ rows: ArrowTable.Snapshot, format: DelimitedFormat, to url: URL
    ) throws {
        let delimiter = format.delimiter
        // Truncates any existing file, and fails here — before a single row has
        // been formatted — if the location cannot be written at all.
        try Data().write(to: url)
        let handle = try FileHandle(forWritingTo: url)
        defer { try? handle.close() }

        var buffer = Data()
        buffer.reserveCapacity(flushBytes * 2)
        let header = rows.columns.map { $0.name as String? }
        buffer.append(contentsOf: row(header, delimiter: delimiter).utf8)

        for r in 0..<rows.rowCount {
            let fields = rows.columns.indices.map { rows.value(row: r, column: $0) }
            buffer.append(contentsOf: row(fields, delimiter: delimiter).utf8)
            if buffer.count >= flushBytes {
                try handle.write(contentsOf: buffer)
                buffer.removeAll(keepingCapacity: true)
            }
        }
        if !buffer.isEmpty { try handle.write(contentsOf: buffer) }
    }
}
