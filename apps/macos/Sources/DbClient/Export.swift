import Foundation
import UniformTypeIdentifiers

/// A format a result can be written to.
///
/// The writing happens in the core, so this is a list of names rather than a
/// list of writers: every case here is a string `db_export` already knows, and
/// a case added without one on the other side would offer the user a format
/// that fails at the save panel.
enum ExportFormat: String, CaseIterable {
    case csv
    case tsv
    case jsonl
    case parquet
    case sql

    var fileExtension: String { rawValue }

    var label: String {
        switch self {
        case .csv: return "CSV"
        case .tsv: return "TSV"
        case .jsonl: return "JSON Lines"
        case .parquet: return "Parquet"
        case .sql: return "SQL"
        }
    }

    /// What the core is told. Separate from `rawValue` only so that renaming a
    /// case here cannot silently change what crosses the boundary.
    var wireName: String { rawValue }

    /// Whether this one is written as INSERT statements, which needs a table to
    /// name and the connection's dialect to spell it in — neither of which the
    /// other four have any use for.
    var needsTable: Bool { self == .sql }

    var contentType: UTType {
        switch self {
        case .csv: return .commaSeparatedText
        case .tsv: return .tabSeparatedText
        // No system type for either, and inventing one would have the panel
        // refuse the extension the core actually writes.
        case .jsonl: return .data
        case .parquet: return .data
        case .sql: return UTType(filenameExtension: "sql") ?? .plainText
        }
    }

    init?(pathExtension: String) {
        guard let match = Self(rawValue: pathExtension.lowercased()) else { return nil }
        self = match
    }
}

/// How much of a result to write.
///
/// Only ever asked when the two differ — a result the grid holds in full has
/// one answer, and offering a choice there is a question with no wrong answer,
/// which is worse than no question.
enum ExportScope {
    /// Everything the statement returns, streamed from the server. The grid's
    /// cap is a property of the grid and not of the data.
    case wholeResult
    /// The first `rows` rows, which is what the window is showing.
    case firstRows(Int)

    /// The limit `db_export` is given. Zero means no limit, which is the
    /// convention on that side because a negative row count is not a thing and
    /// `Option` does not cross the C boundary.
    var rowLimit: Int64 {
        switch self {
        case .wholeResult: return 0
        case .firstRows(let rows): return Int64(rows)
        }
    }
}
